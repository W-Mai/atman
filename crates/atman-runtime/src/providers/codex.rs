use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable, TurnId};
use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
use crate::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, ModelDiscoveryError, Provider,
    ReasoningEffort, ReasoningSelection, ReasoningWireProfile, StopReason, TokenUsage,
    estimate_tokens,
};
use crate::tool::BoxFut;
use anyhow::Context;

const CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/wham/models";
const X_CODEX_TURN_STATE: &str = "x-codex-turn-state";
const MAX_RETAINED_TURN_STATES: usize = 256;

#[derive(Clone)]
enum CodexCredentialSource {
    Static {
        access_token: String,
        account_id: String,
    },
    Managed(crate::oauth::OAuthCredentialLease),
}

struct CodexRequestCredentials {
    access_token: String,
    account_id: String,
}

impl CodexCredentialSource {
    async fn acquire(&self) -> Result<CodexRequestCredentials, crate::oauth::OAuthCredentialError> {
        match self {
            Self::Static {
                access_token,
                account_id,
            } => Ok(CodexRequestCredentials {
                access_token: access_token.clone(),
                account_id: account_id.clone(),
            }),
            Self::Managed(lease) => {
                let credential = lease.acquire().await?;
                let account_id =
                    oauth_account_id(&credential.access_token, credential.display_account);
                Ok(CodexRequestCredentials {
                    access_token: credential.access_token,
                    account_id,
                })
            }
        }
    }
}

fn oauth_account_id(access_token: &str, legacy_account: Option<String>) -> String {
    crate::oauth::extract_chatgpt_account_id(access_token)
        .or_else(|| {
            legacy_account.filter(|account| {
                let account = account.trim();
                !account.is_empty() && !account.contains('@')
            })
        })
        .unwrap_or_default()
}

/// ChatGPT backend provider. Requires `originator: codex_cli_rs` header for Cloudflare.
pub struct CodexProvider {
    name: String,
    credentials: CodexCredentialSource,
    client: reqwest::Client,
    responses_url: String,
    models_url: String,
    turn_states: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl CodexProvider {
    pub fn new(
        name: impl Into<String>,
        access_token: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            credentials: CodexCredentialSource::Static {
                access_token: access_token.into(),
                account_id: account_id.into(),
            },
            client: reqwest::Client::new(),
            responses_url: format!("{CODEX_BASE}/responses"),
            models_url: CODEX_MODELS_URL.into(),
            turn_states: Default::default(),
        }
    }

    fn from_oauth_store(
        stored: &crate::auth_store::StoredProvider,
        hub: crate::config_hub::ConfigHub,
    ) -> Self {
        Self {
            name: stored.id.clone(),
            credentials: CodexCredentialSource::Managed(crate::oauth::OAuthCredentialLease::new::<
                Self,
            >(&stored.id, hub)),
            client: reqwest::Client::new(),
            responses_url: format!("{CODEX_BASE}/responses"),
            models_url: CODEX_MODELS_URL.into(),
            turn_states: Default::default(),
        }
    }

    #[cfg(test)]
    fn with_endpoints(
        mut self,
        responses_url: impl Into<String>,
        models_url: impl Into<String>,
    ) -> Self {
        self.responses_url = responses_url.into();
        self.models_url = models_url.into();
        self
    }

    fn build_body(&self, req: &LlmRequest) -> Result<ResponsesRequest, RuntimeError> {
        let model = req
            .model
            .split_once('/')
            .map(|(_, slug)| slug)
            .or_else(|| req.model.split_once(':').map(|(_, slug)| slug))
            .unwrap_or(&req.model)
            .to_string();

        let input = build_input_items(req)?;
        let tools = build_tools(&req.tools);

        Ok(ResponsesRequest {
            model,
            input,
            instructions: req.system.clone(),
            tools,
            stream: true,
            store: false,
            reasoning: build_reasoning_config(&req.reasoning),
            text: Some(TextConfig {
                verbosity: "medium".into(),
            }),
            include: Some(vec!["reasoning.encrypted_content".into()]),
            prompt_cache_key: req.prompt_cache_key.clone(),
        })
    }

    fn validate_reasoning(selection: &ReasoningSelection) -> Result<(), RuntimeError> {
        ReasoningWireProfile::CodexResponses
            .validate(selection, None)
            .map_err(|error| RuntimeError::ToolFailed(format!("invalid request: {error}")))
    }
}

fn build_reasoning_config(selection: &ReasoningSelection) -> Option<ReasoningConfig> {
    match selection {
        ReasoningSelection::ProviderDefault => None,
        ReasoningSelection::Disabled => Some(ReasoningConfig {
            effort: Some(ReasoningEffort::None.to_string()),
            mode: None,
            summary: None,
        }),
        ReasoningSelection::Auto { execution_mode } => Some(ReasoningConfig {
            effort: None,
            mode: execution_mode.as_ref().map(ToString::to_string),
            summary: Some("auto".into()),
        }),
        ReasoningSelection::Effort {
            effort,
            execution_mode,
        } => Some(ReasoningConfig {
            effort: Some(effort.to_string()),
            mode: execution_mode.as_ref().map(ToString::to_string),
            summary: (!matches!(effort, ReasoningEffort::None)).then(|| "auto".into()),
        }),
        ReasoningSelection::BudgetTokens { .. } => None,
    }
}

fn build_input_items(req: &LlmRequest) -> Result<Vec<InputItem>, RuntimeError> {
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in &req.messages {
        if m.role == MessageRole::Assistant {
            for p in &m.parts {
                if let MessagePart::ToolUse { id, name, .. } = p {
                    tool_names.insert(id.clone(), crate::tool_naming::to_wire(name));
                }
            }
        }
    }

    let mut items: Vec<InputItem> = Vec::new();

    for m in &req.messages {
        match m.role {
            MessageRole::User => {
                let content = build_user_content(&m.parts)?;
                items.push(InputItem {
                    role: Some("user".into()),
                    content: Some(content),
                    item_type: Some("message".into()),
                    call_id: None,
                    name: None,
                    arguments: None,
                    output: None,
                });
            }
            MessageRole::Assistant => {
                let (text, tool_calls) = split_assistant_parts(&m.parts, &req.tools);
                if let Some(t) = text {
                    items.push(InputItem {
                        role: Some("assistant".into()),
                        content: Some(InputContent::Text(t)),
                        item_type: Some("message".into()),
                        call_id: None,
                        name: None,
                        arguments: None,
                        output: None,
                    });
                }
                for tc in tool_calls {
                    items.push(InputItem {
                        role: None,
                        content: None,
                        item_type: Some("function_call".into()),
                        call_id: Some(tc.id),
                        name: Some(tc.name),
                        arguments: Some(tc.arguments),
                        output: None,
                    });
                }
            }
            MessageRole::Tool => {
                for p in &m.parts {
                    if let MessagePart::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = p
                    {
                        let name = tool_names.get(tool_use_id).cloned();
                        items.push(InputItem {
                            role: None,
                            content: None,
                            item_type: Some("function_call_output".into()),
                            call_id: Some(tool_use_id.clone()),
                            name,
                            arguments: None,
                            output: Some(content.clone()),
                        });
                    }
                }
            }
            MessageRole::System => {
                let content = build_user_content(&m.parts)?;
                items.push(InputItem {
                    role: Some(
                        if m.origin == MessageOrigin::Internal
                            && m.parts
                                .iter()
                                .any(|part| matches!(part, MessagePart::ContextRecord(_)))
                        {
                            "developer"
                        } else {
                            "user"
                        }
                        .into(),
                    ),
                    content: Some(content),
                    item_type: Some("message".into()),
                    call_id: None,
                    name: None,
                    arguments: None,
                    output: None,
                });
            }
        }
    }

    Ok(items)
}

fn build_user_content(parts: &[MessagePart]) -> Result<InputContent, RuntimeError> {
    let mut parts_out: Vec<ResponseInputContent> = Vec::new();
    for p in parts {
        match p {
            MessagePart::ContextRecord(record) => {
                parts_out.push(ResponseInputContent::InputText {
                    text: record.render_for_model(),
                });
            }
            MessagePart::Text { text } => {
                parts_out.push(ResponseInputContent::InputText { text: text.clone() });
            }
            MessagePart::Image { source } => {
                let data = crate::attachment_store::image_base64(source)?;
                parts_out.push(ResponseInputContent::InputImage {
                    image_url: format!("data:{};base64,{}", source.media_type, data),
                    detail: (!matches!(source.detail, crate::provider::ImageDetail::Auto))
                        .then(|| source.detail.as_str()),
                });
            }
            MessagePart::CompactSummary { summary, .. } => {
                parts_out.push(ResponseInputContent::InputText {
                    text: summary.clone(),
                });
            }
            _ => {}
        }
    }
    if let [ResponseInputContent::InputText { text }] = parts_out.as_slice() {
        Ok(InputContent::Text(text.clone()))
    } else {
        Ok(InputContent::Parts(parts_out))
    }
}

struct AssistantSplit {
    id: String,
    name: String,
    arguments: String,
}

fn split_assistant_parts(
    parts: &[MessagePart],
    tool_specs: &[crate::tool::ToolSpec],
) -> (Option<String>, Vec<AssistantSplit>) {
    let mut text = String::new();
    let mut tools: Vec<AssistantSplit> = Vec::new();
    for p in parts {
        match p {
            MessagePart::ContextRecord(record) => text.push_str(&record.render_for_model()),
            MessagePart::Text { text: t } => text.push_str(t),
            MessagePart::ToolUse {
                id,
                name,
                input,
                intent,
            } => tools.push(AssistantSplit {
                id: id.clone(),
                name: crate::tool_naming::to_wire(name),
                arguments: serde_json::to_string(&crate::message::encode_tool_call_input(
                    input,
                    intent.as_ref(),
                    name,
                    tool_specs,
                ))
                .unwrap_or_default(),
            }),
            _ => {}
        }
    }
    let text_out = if text.is_empty() { None } else { Some(text) };
    (text_out, tools)
}

fn build_tools(tools: &[crate::tool::ToolSpec]) -> Vec<ResponsesTool> {
    tools
        .iter()
        .map(|t| ResponsesTool {
            r#type: "function".into(),
            // Responses function names reject '.', so use the provider-safe mapping.
            name: crate::tool_naming::to_wire(&t.name),
            description: t.description.clone(),
            parameters: t.input_schema.clone(),
        })
        .collect()
}

#[derive(Deserialize)]
struct CodexModelsResponse {
    models: Vec<CodexModelResponse>,
}

#[derive(Deserialize)]
struct CodexModelResponse {
    slug: String,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexReasoningLevel>,
    #[serde(default)]
    default_reasoning_level: Option<ReasoningEffort>,
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CodexReasoningLevel {
    Name(String),
    Detail {
        effort: String,
        #[serde(default, rename = "description")]
        _description: Option<String>,
    },
}

impl CodexReasoningLevel {
    fn effort(self) -> String {
        match self {
            Self::Name(effort) | Self::Detail { effort, .. } => effort,
        }
    }
}

fn parse_codex_models(
    bytes: &[u8],
) -> Result<Vec<crate::provider::DiscoveredModelDetails>, ModelDiscoveryError> {
    let response: CodexModelsResponse = serde_json::from_slice(bytes)
        .map_err(|error| ModelDiscoveryError::InvalidResponse(error.to_string()))?;
    response
        .models
        .into_iter()
        .enumerate()
        .map(|(index, model)| {
            let raw_slug = model.slug.trim();
            if raw_slug.is_empty() {
                return Err(ModelDiscoveryError::InvalidResponse(format!(
                    "models[{index}].slug must not be empty"
                )));
            }
            let slug = if raw_slug.starts_with("codex/") {
                raw_slug.to_string()
            } else {
                format!("codex/{raw_slug}")
            };
            let reasoning_efforts = model
                .supported_reasoning_levels
                .into_iter()
                .map(|level| {
                    let raw = level.effort();
                    raw.parse().map_err(|error| {
                        ModelDiscoveryError::InvalidResponse(format!(
                            "models[{index}] has invalid reasoning effort `{raw}`: {error}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let input_modalities = model
                .input_modalities
                .into_iter()
                .filter_map(|value| match value.as_str() {
                    "text" => Some(crate::provider::InputModality::Text),
                    "image" => Some(crate::provider::InputModality::Image),
                    "audio" => Some(crate::provider::InputModality::Audio),
                    _ => None,
                })
                .collect();
            Ok(crate::provider::DiscoveredModelDetails {
                slug,
                context_budget: model.context_window,
                capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                    crate::provider::ModelCapabilities {
                        reasoning_efforts,
                        default_reasoning_effort: model.default_reasoning_level,
                        input_modalities,
                        ..Default::default()
                    },
                ),
            })
        })
        .collect()
}

fn discovery_error_body(body: &str) -> String {
    body.chars().take(512).collect()
}

impl Provider for CodexProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            prompt_cache_key: true,
            context_prefix_profile: crate::context_plan::ContextPrefixProfile::CodexResponses,
        }
    }

    fn context_prefix(
        &self,
        req: &LlmRequest,
    ) -> Result<crate::context_plan::ContextPrefixSnapshot, RuntimeError> {
        let body = self.build_body(req)?;
        let mut builder = crate::context_plan::ContextPrefixSnapshot::builder(
            crate::context_plan::ContextPrefixProfile::CodexResponses,
            req,
        );
        if let Some(instructions) = &body.instructions {
            builder.push(crate::context_plan::ContextPrefixLane::Stable, instructions)?;
        }
        for tool in &body.tools {
            builder.push(crate::context_plan::ContextPrefixLane::Tools, tool)?;
        }
        for item in &body.input {
            builder.push(
                if item.role.as_deref() == Some("developer") {
                    crate::context_plan::ContextPrefixLane::Records
                } else {
                    crate::context_plan::ContextPrefixLane::Messages
                },
                item,
            )?;
        }
        Ok(builder.finish())
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        // The Codex backend always uses streaming (Responses API with store=false).
        // We call streaming internally and collect the result.
        let observable = self.call_streaming(req);
        Box::pin(async move {
            // Drop the broadcast events — callers of `call()` don't consume them.
            let _events = observable.events;
            observable.output.await
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let preflight =
            Self::validate_reasoning(&req.reasoning).and_then(|()| self.build_body(&req));
        let turn_id = turn_id_from_req(&req);
        let streaming_tools = req.tools.clone();
        let credentials = self.credentials.clone();
        let client = self.client.clone();
        let responses_url = self.responses_url.clone();
        let turn_states = self.turn_states.clone();
        let routing_turn_id = req
            .messages
            .last()
            .map(|message| message.turn_id.to_string())
            .unwrap_or_else(|| turn_id.to_string());
        let turn_state_key = req
            .prompt_cache_key
            .as_ref()
            .map(|routing_key| format!("{routing_key}:{routing_turn_id}"));
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();

        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> = Box::pin(
            async move {
                let body = preflight?;
                let routing_key = body.prompt_cache_key.clone();
                let credentials = tokio::select! {
                    biased;
                    _ = cancel_for_task.cancelled() => {
                        return Err(RuntimeError::Cancelled("codex cancelled before authentication".into()));
                    }
                    result = credentials.acquire() => result.map_err(credential_err)?,
                };
                let mut request = client
                    .post(responses_url)
                    .bearer_auth(credentials.access_token)
                    .header("originator", "codex_cli_rs")
                    .header("OpenAI-Beta", "responses=experimental")
                    .header("accept", "text/event-stream")
                    .json(&body);
                if !credentials.account_id.is_empty() {
                    request = request.header("chatgpt-account-id", credentials.account_id);
                }
                if let Some(routing_key) = routing_key.as_deref() {
                    request = request
                        .header("session-id", routing_key)
                        .header("thread-id", routing_key)
                        .header("x-client-request-id", routing_key);
                }
                if let Some(turn_state_key) = turn_state_key.as_deref()
                    && let Some(turn_state) = turn_states
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(turn_state_key)
                        .cloned()
                {
                    request = request.header(X_CODEX_TURN_STATE, turn_state);
                }
                use eventsource_stream::Eventsource;
                use futures::StreamExt;

                let resp = tokio::select! {
                    biased;
                    _ = cancel_for_task.cancelled() => {
                        return Err(RuntimeError::Cancelled("codex cancelled before send".into()));
                    }
                    r = request.send() => r.map_err(net_err)?,
                };
                let status = resp.status();
                let response_turn_state = resp
                    .headers()
                    .get(X_CODEX_TURN_STATE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                if !status.is_success() {
                    let body_text = resp.text().await.unwrap_or_default();
                    if let Some(reason) =
                        super::classify_attachment_error(status.as_u16(), &body_text)
                    {
                        return Err(RuntimeError::AttachmentError { reason });
                    }
                    return Err(RuntimeError::ToolFailed(format!(
                        "codex http {status}: {body_text}"
                    )));
                }

                if let (Some(turn_state_key), Some(turn_state)) =
                    (turn_state_key, response_turn_state)
                {
                    let mut states = turn_states
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !states.contains_key(&turn_state_key)
                        && states.len() >= MAX_RETAINED_TURN_STATES
                    {
                        states.clear();
                    }
                    states.entry(turn_state_key).or_insert(turn_state);
                }

                let mut stream = resp.bytes_stream().eventsource();

                let mut acc_text = String::new();
                let mut acc_thinking = String::new();
                let mut cumulative = 0u64;
                let mut final_usage: Option<ResponsesUsage> = None;
                let mut resp_model: Option<String> = None;
                let mut resp_id: Option<String> = None;
                let mut stop_reason = StopReason::End;

                let mut partial_tool_calls: Vec<PartialToolCall> = Vec::new();

                while let Some(event) = tokio::select! {
                    biased;
                    _ = cancel_for_task.cancelled() => None,
                    next = stream.next() => next,
                } {
                    let event =
                        event.map_err(|e| RuntimeError::ToolFailed(format!("codex sse: {e}")))?;
                    if event.data.is_empty() || event.data == "[DONE]" {
                        continue;
                    }
                    let parsed: serde_json::Value = match serde_json::from_str(&event.data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let ev_type = parsed["type"].as_str().unwrap_or("");

                    match ev_type {
                        "response.output_text.delta" => {
                            if let Some(delta) = parsed["delta"].as_str() {
                                acc_text.push_str(delta);
                                cumulative += estimate_tokens(delta);
                                let _ = tx.send(NodeEvent::LlmChunk {
                                    text: delta.to_string(),
                                    cumulative_tokens: cumulative,
                                });
                            }
                        }

                        "response.reasoning_text.delta" => {
                            if let Some(delta) = parsed["delta"].as_str() {
                                acc_thinking.push_str(delta);
                                let _ = tx.send(NodeEvent::ThinkingChunk {
                                    text: delta.to_string(),
                                });
                            }
                        }

                        "response.output_item.added" => {
                            if let Some(item) = parsed.get("item")
                                && item["type"].as_str() == Some("function_call")
                            {
                                let idx = parsed["output_index"].as_u64().unwrap_or(0) as usize;
                                while partial_tool_calls.len() <= idx {
                                    partial_tool_calls.push(PartialToolCall::default());
                                }
                                let slot = &mut partial_tool_calls[idx];
                                slot.id = item["call_id"].as_str().unwrap_or("").to_string();
                                slot.name = item["name"].as_str().unwrap_or("").to_string();
                            }
                        }

                        "response.function_call_arguments.delta" => {
                            let idx = parsed["output_index"].as_u64().unwrap_or(0) as usize;
                            while partial_tool_calls.len() <= idx {
                                partial_tool_calls.push(PartialToolCall::default());
                            }
                            if let Some(delta) = parsed["delta"].as_str() {
                                partial_tool_calls[idx].arguments.push_str(delta);
                            }
                        }

                        "response.completed" => {
                            if let Some(r) = parsed.get("response") {
                                resp_model = r["model"].as_str().map(|s| s.to_string());
                                resp_id = r["id"].as_str().map(|s| s.to_string());
                                if let Some(u) = r.get("usage") {
                                    final_usage =
                                        serde_json::from_value::<ResponsesUsage>(u.clone()).ok();
                                }
                                if r["status"].as_str() == Some("cancelled") {
                                    stop_reason = StopReason::Cancelled;
                                }
                            }
                        }

                        "error" => {
                            let msg = parsed["message"].as_str().unwrap_or("unknown codex error");
                            return Err(RuntimeError::ToolFailed(msg.to_string()));
                        }

                        _ => {}
                    }
                }

                if cancel_for_task.is_cancelled() {
                    let _ = tx.send(NodeEvent::LlmDone {
                        total_tokens: cumulative,
                    });
                    return Err(RuntimeError::Cancelled("codex cancelled mid-stream".into()));
                }

                let total_output = final_usage
                    .as_ref()
                    .and_then(|u| u.output_tokens)
                    .unwrap_or(cumulative);
                let _ = tx.send(NodeEvent::LlmDone {
                    total_tokens: total_output,
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
                    if tc.name.is_empty() {
                        continue;
                    }
                    let input: serde_json::Value = if tc.arguments.is_empty() {
                        serde_json::Value::Object(Default::default())
                    } else {
                        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null)
                    };
                    let name = crate::tool_naming::from_wire(&tc.name, &streaming_tools);
                    let (input, intent) =
                        crate::message::decode_tool_call_input(input, &name, &streaming_tools);
                    parts.push(MessagePart::ToolUse {
                        id: tc.id,
                        name,
                        input,
                        intent,
                    });
                }

                let token_usage = final_usage.map(|u| {
                    let input_tokens = u.input_tokens.unwrap_or(0);
                    let cached_input = u
                        .input_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0);
                    let cache_write = u
                        .input_tokens_details
                        .as_ref()
                        .and_then(|d| d.cache_write_tokens)
                        .unwrap_or(0);
                    TokenUsage {
                        // Responses API reports input_tokens as the total input,
                        // including cache reads and writes. TokenUsage stores
                        // the three prompt lanes independently.
                        input: crate::provider::regular_input_tokens(
                            input_tokens,
                            cached_input,
                            cache_write,
                        ),
                        cached_input,
                        output: u.output_tokens.unwrap_or(0),
                        cache_write,
                        reasoning_tokens: u
                            .output_tokens_details
                            .as_ref()
                            .and_then(|d| d.reasoning_tokens)
                            .unwrap_or(0),
                    }
                });

                Ok(AssistantMessage {
                    message: Message {
                        role: MessageRole::Assistant,
                        parts,
                        turn_id,
                        origin: MessageOrigin::User,
                    },
                    stop_reason,
                    token_usage: token_usage.unwrap_or_default(),
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
    ) -> crate::tool::BoxFut<'static, Vec<crate::provider::DiscoveredModel>> {
        let discovery = self.try_discover_models();
        Box::pin(async move {
            discovery
                .await
                .unwrap_or_default()
                .into_iter()
                .map(crate::provider::DiscoveredModel::from)
                .collect()
        })
    }

    fn try_discover_models(
        &self,
    ) -> crate::tool::BoxFut<
        'static,
        Result<Vec<crate::provider::DiscoveredModelDetails>, ModelDiscoveryError>,
    > {
        let credentials = self.credentials.clone();
        let client = self.client.clone();
        let models_url = self.models_url.clone();
        Box::pin(async move {
            let credentials = credentials.acquire().await.map_err(|error| {
                ModelDiscoveryError::Transport(format!("codex credentials: {error}"))
            })?;
            let mut request = client
                .get(models_url)
                .query(&[("client_version", "0.0.0")])
                .bearer_auth(credentials.access_token);
            if !credentials.account_id.is_empty() {
                request = request.header("ChatGPT-Account-Id", credentials.account_id);
            }
            let resp = request
                .send()
                .await
                .map_err(|error| ModelDiscoveryError::Transport(error.to_string()))?;
            let status = resp.status();
            let bytes = resp
                .bytes()
                .await
                .map_err(|error| ModelDiscoveryError::Transport(error.to_string()))?;
            if !status.is_success() {
                return Err(ModelDiscoveryError::Http {
                    status: status.as_u16(),
                    body: discovery_error_body(&String::from_utf8_lossy(&bytes)),
                });
            }
            parse_codex_models(&bytes)
        })
    }

    fn test_connection(&self) -> BoxFut<'_, Result<String, String>> {
        let credentials = self.credentials.clone();
        let models_url = self.models_url.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let credentials = credentials
                .acquire()
                .await
                .map_err(|error| format!("credentials unavailable — {error}"))?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|e| e.to_string())?;
            let mut request = client
                .get(models_url)
                .query(&[("client_version", "0.0.0")])
                .bearer_auth(credentials.access_token);
            if !credentials.account_id.is_empty() {
                request = request.header("ChatGPT-Account-Id", credentials.account_id);
            }
            let resp = request
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

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

impl crate::oauth::OAuthProvider for CodexProvider {
    const KIND: crate::auth_store::ProviderKind = crate::auth_store::ProviderKind::Codex;

    fn authorize_url() -> (String, crate::oauth::Pkce, String) {
        let pkce = crate::oauth::Pkce::generate();
        let state = crate::oauth::generate_state();
        let url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}&scope=openid+profile+email+offline_access",
            CODEX_AUTHORIZE_URL, CODEX_CLIENT_ID, CODEX_REDIRECT_URI, pkce.challenge, state
        );
        (url, pkce, state)
    }

    fn exchange_code(
        code: &str,
        verifier: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::oauth::TokenResult>> + Send>,
    > {
        let code = code.to_string();
        let verifier = verifier.to_string();
        Box::pin(async move {
            let client = reqwest::Client::new();
            let resp = client
                .post(CODEX_TOKEN_URL)
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", CODEX_REDIRECT_URI),
                    ("client_id", CODEX_CLIENT_ID),
                    ("code_verifier", &verifier),
                ])
                .send()
                .await
                .context("token exchange request")?;

            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("token exchange failed (HTTP {status}): {body_text}");
            }

            #[derive(serde::Deserialize)]
            struct R {
                access_token: String,
                refresh_token: Option<String>,
                id_token: Option<String>,
            }
            let data: R = serde_json::from_str(&body_text).context("parse token response")?;

            let expires_at = crate::oauth::parse_jwt_exp(&data.access_token)
                .unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600);
            let account = data
                .id_token
                .as_deref()
                .and_then(crate::oauth::extract_account_from_id_token);

            Ok(crate::oauth::TokenResult {
                access_token: data.access_token,
                refresh_token: data.refresh_token,
                expires_at,
                account,
            })
        })
    }

    fn refresh_token(
        token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::oauth::TokenResult>> + Send>,
    > {
        let token = token.to_string();
        Box::pin(async move {
            let client = reqwest::Client::new();
            let resp = client
                .post(CODEX_TOKEN_URL)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &token),
                    ("client_id", CODEX_CLIENT_ID),
                ])
                .send()
                .await
                .context("token refresh request")?;

            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("token refresh failed (HTTP {status}): {body_text}");
            }

            #[derive(serde::Deserialize)]
            struct R {
                access_token: String,
                refresh_token: Option<String>,
                id_token: Option<String>,
            }
            let data: R = serde_json::from_str(&body_text).context("parse refresh response")?;

            let expires_at = crate::oauth::parse_jwt_exp(&data.access_token)
                .unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600);
            let account = data
                .id_token
                .as_deref()
                .and_then(crate::oauth::extract_account_from_id_token);

            Ok(crate::oauth::TokenResult {
                access_token: data.access_token,
                refresh_token: data.refresh_token,
                expires_at,
                account,
            })
        })
    }

    fn from_stored(stored: &crate::auth_store::StoredProvider) -> Self {
        let account_id = oauth_account_id(&stored.access_token, stored.account.clone());
        CodexProvider::new(&stored.id, &stored.access_token, account_id)
    }

    fn from_managed_stored(
        stored: &crate::auth_store::StoredProvider,
        hub: crate::config_hub::ConfigHub,
    ) -> Option<Self> {
        Some(CodexProvider::from_oauth_store(stored, hub))
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn turn_id_from_req(req: &LlmRequest) -> TurnId {
    req.messages
        .first()
        .map(|m| m.turn_id.clone())
        .unwrap_or_else(TurnId::now)
}

fn net_err(e: reqwest::Error) -> RuntimeError {
    RuntimeError::ToolFailed(format!("codex net: {e}"))
}

fn credential_err(error: crate::oauth::OAuthCredentialError) -> RuntimeError {
    RuntimeError::ToolFailed(format!("codex credentials: {error}"))
}

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesTool>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
}

#[derive(Serialize)]
struct InputItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<InputContent>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    item_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum InputContent {
    Text(String),
    Parts(Vec<ResponseInputContent>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseInputContent {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<&'static str>,
    },
}

#[derive(Serialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    r#type: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

#[derive(Serialize)]
struct TextConfig {
    verbosity: String,
}

#[derive(Deserialize, Default)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Deserialize, Default)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{
        CodexCredentialSource, CodexProvider, X_CODEX_TURN_STATE, oauth_account_id,
        parse_codex_models, split_assistant_parts,
    };
    use crate::message::MessagePart;
    use crate::provider::Provider;
    use base64::Engine;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
    fn tool_call_intent_is_serialized_into_function_arguments() {
        let tools = vec![crate::tool::tool_spec(&IntentTool)];
        let (_, calls) = split_assistant_parts(
            &[MessagePart::ToolUse {
                id: "call-1".into(),
                name: "probe".into(),
                input: serde_json::json!({"value": 1}),
                intent: crate::message::ToolCallIntent::new("Inspect provider state"),
            }],
            &tools,
        );
        let arguments: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(arguments["value"], 1);
        assert_eq!(arguments["_atman_intent"], "Inspect provider state");
    }

    #[test]
    fn context_prefix_uses_responses_projection_and_preserves_appended_messages() {
        let (_dir, _hub, provider, _) = managed_provider(
            "http://localhost/responses".into(),
            "http://localhost/models".into(),
        );
        let mut request = request();
        request.cache_prompt = true;
        request.system = Some("stable".into());
        request.messages.push(crate::message::Message::user_text(
            crate::event::TurnId::now(),
            "first",
        ));
        let first = provider.context_prefix(&request).unwrap();
        let first_bytes = first.initial_observation().wire_prefix_bytes;
        request
            .messages
            .push(crate::message::Message::assistant_text(
                crate::event::TurnId::now(),
                "second",
            ));
        let second = provider.context_prefix(&request).unwrap();
        let observation = second.compare("codex", "codex", "model", "model", &first);

        assert_eq!(
            observation.profile,
            crate::context_plan::ContextPrefixProfile::CodexResponses
        );
        assert_eq!(observation.reset_reason, None);
        assert_eq!(observation.common_prefix_bytes, first_bytes);
    }

    #[test]
    fn responses_request_serializes_prompt_cache_key() {
        let provider = CodexProvider::new("codex", "token", "account");
        let mut request = request();
        request.cache_prompt = true;
        request.prompt_cache_key = Some("atman-route".into());

        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert_eq!(body["prompt_cache_key"], "atman-route");
        assert!(provider.capabilities().prompt_cache_key);
    }

    fn request() -> crate::provider::LlmRequest {
        crate::provider::LlmRequest {
            model: "codex/gpt-test".into(),
            messages: Vec::new(),
            system: None,
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: false,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        }
    }

    fn managed_provider(
        responses_url: String,
        models_url: String,
    ) -> (
        tempfile::TempDir,
        crate::config_hub::ConfigHub,
        CodexProvider,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let hub = crate::config_hub::ConfigHub::from_config_dir(dir.path());
        hub.add_auth_provider(crate::auth_store::StoredProvider {
            id: "oauth-account".into(),
            name: "OAuth account".into(),
            kind: crate::auth_store::ProviderKind::Codex,
            access_token: "access-v1".into(),
            refresh_token: Some("refresh-v1".into()),
            expires_at: chrono::Utc::now().timestamp() - 1,
            account: Some("display@example.test".into()),
            enabled: true,
            model_cache: None,
        })
        .unwrap();
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-v2"}}"#);
        let access_token = format!("header.{payload}.signature");
        let refreshed_access_token = access_token.clone();
        let lease = crate::oauth::OAuthCredentialLease::with_refresher(
            "oauth-account",
            crate::auth_store::ProviderKind::Codex,
            hub.clone(),
            move |refresh_token| {
                assert_eq!(refresh_token, "refresh-v1");
                let access_token = refreshed_access_token.clone();
                Box::pin(async move {
                    Ok(crate::oauth::TokenResult {
                        access_token,
                        refresh_token: Some("refresh-v2".into()),
                        expires_at: chrono::Utc::now().timestamp() + 3_600,
                        account: Some("display-v2@example.test".into()),
                    })
                })
            },
        );
        let provider = CodexProvider {
            name: "oauth-account".into(),
            credentials: CodexCredentialSource::Managed(lease),
            client: reqwest::Client::new(),
            responses_url: String::new(),
            models_url: String::new(),
            turn_states: Default::default(),
        }
        .with_endpoints(responses_url, models_url);
        (dir, hub, provider, access_token)
    }

    async fn mount_models_endpoint(server: &MockServer, access_token: &str) {
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", format!("Bearer {access_token}")))
            .and(header("chatgpt-account-id", "account-v2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{
                    "slug": "gpt-test",
                    "supported_reasoning_levels": ["low", "high"]
                }]
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[test]
    fn input_tokens_exclude_cached_tokens_for_window_accounting() {
        assert_eq!(
            crate::provider::regular_input_tokens(100_000, 60_000, 10_000),
            30_000
        );
    }

    #[test]
    fn display_email_is_not_used_as_chatgpt_account_id() {
        assert_eq!(
            oauth_account_id("not-a-jwt", Some("display@example.test".into())),
            ""
        );
        assert_eq!(
            oauth_account_id("not-a-jwt", Some("legacy-account-id".into())),
            "legacy-account-id"
        );
    }

    #[test]
    fn cached_tokens_cannot_underflow_input_tokens() {
        assert_eq!(crate::provider::regular_input_tokens(10, 20, 5), 0);
    }

    #[test]
    fn model_catalog_parses_object_reasoning_levels() {
        let models = parse_codex_models(
            br#"{
                "models": [{
                    "slug": "gpt-test",
                    "context_window": 272000,
                    "supported_reasoning_levels": [
                        {"effort":"low","description":"Fast"},
                        {"effort":"medium","description":"Balanced"},
                        {"effort":"high","description":"Deep"},
                        {"effort":"xhigh","description":"Deeper"},
                        {"effort":"max","description":"Maximum"},
                        {"effort":"ultra","description":"Extended"}
                    ],
                    "default_reasoning_level": "medium",
                    "input_modalities": ["text", "image"]
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "codex/gpt-test");
        assert_eq!(models[0].context_budget, Some(272_000));
        assert!(models[0].capability_knowledge.thinking());
        let capabilities = models[0].capability_knowledge.advertised().unwrap();
        assert_eq!(
            capabilities.reasoning_efforts,
            vec![
                crate::provider::ReasoningEffort::Low,
                crate::provider::ReasoningEffort::Medium,
                crate::provider::ReasoningEffort::High,
                crate::provider::ReasoningEffort::XHigh,
                crate::provider::ReasoningEffort::Max,
                crate::provider::ReasoningEffort::Ultra,
            ]
        );
        assert_eq!(
            capabilities.default_reasoning_effort,
            Some(crate::provider::ReasoningEffort::Medium)
        );
        assert_eq!(
            capabilities.input_modalities,
            vec![
                crate::provider::InputModality::Text,
                crate::provider::InputModality::Image,
            ]
        );
    }

    #[test]
    fn model_catalog_accepts_legacy_string_reasoning_levels() {
        let models = parse_codex_models(
            br#"{
                "models": [{
                    "slug": "codex/legacy-test",
                    "supported_reasoning_levels": ["low", "high"]
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(models[0].slug, "codex/legacy-test");
        assert_eq!(
            models[0]
                .capability_knowledge
                .advertised()
                .unwrap()
                .reasoning_efforts,
            vec![
                crate::provider::ReasoningEffort::Low,
                crate::provider::ReasoningEffort::High,
            ]
        );
    }

    #[test]
    fn malformed_model_catalog_is_not_treated_as_an_empty_catalog() {
        let error = parse_codex_models(br#"{"unexpected":[]}"#).unwrap_err();

        assert!(matches!(
            error,
            crate::provider::ModelDiscoveryError::InvalidResponse(_)
        ));
    }

    #[test]
    fn reasoning_effort_and_mode_are_not_hardcoded() {
        let provider = CodexProvider::new("codex", "token", "account");
        let request = crate::provider::LlmRequest {
            model: "codex/gpt-test".into(),
            messages: Vec::new(),
            system: None,
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: false,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::Effort {
                effort: crate::provider::ReasoningEffort::XHigh,
                execution_mode: Some(crate::provider::ReasoningExecutionMode::Pro),
            },
            stall_timeout_secs: 0,
        };
        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["mode"], "pro");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn provider_default_omits_reasoning_instead_of_forcing_medium() {
        let provider = CodexProvider::new("codex", "token", "account");
        let request = request();
        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn mixed_user_content_is_a_typed_array_not_a_json_string() {
        use base64::Engine;

        let provider = CodexProvider::new("codex", "token", "account");
        let image = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\n");
        let request = crate::provider::LlmRequest {
            model: "codex/gpt-test".into(),
            messages: vec![crate::message::Message {
                role: crate::message::MessageRole::User,
                parts: vec![
                    crate::message::MessagePart::Image {
                        source: crate::message::ImageSource {
                            media_type: "image/png".into(),
                            data: crate::message::ImageData::Base64 { data: image },
                            detail: crate::provider::ImageDetail::High,
                        },
                    },
                    crate::message::MessagePart::Text {
                        text: "describe".into(),
                    },
                ],
                turn_id: crate::event::TurnId::now(),
                origin: crate::message::MessageOrigin::User,
            }],
            system: None,
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: false,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };

        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert!(body["input"][0]["content"].is_array());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(body["input"][0]["content"][0]["detail"], "high");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_text");
    }

    #[test]
    fn compact_summary_is_preserved_as_input_context() {
        let provider = CodexProvider::new("codex", "token", "account");
        let request = crate::provider::LlmRequest {
            model: "codex/gpt-test".into(),
            messages: vec![crate::message::Message::system_compact_summary(
                crate::event::TurnId::now(),
                "retained summary",
                1,
                9,
                9,
            )],
            system: Some("stable instructions".into()),
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: true,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };

        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert_eq!(body["instructions"], "stable instructions");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "retained summary");
    }

    #[test]
    fn internal_context_record_projects_as_developer_input() {
        let provider = CodexProvider::new("codex", "token", "account");
        let mut request = crate::provider::LlmRequest {
            model: "codex/gpt-test".into(),
            messages: vec![crate::message::Message::user_text(
                crate::event::TurnId::now(),
                "before",
            )],
            system: Some("stable instructions".into()),
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: true,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };
        let before = provider.context_prefix(&request).unwrap();
        let before_bytes = before.initial_observation().wire_prefix_bytes;
        request
            .messages
            .push(crate::message::Message::context_record(
                crate::event::TurnId::now(),
                crate::context_plan::ContextRecord::new(
                    "session.goal",
                    1,
                    crate::context_plan::ContextRecordAuthority::User,
                    crate::context_plan::ContextRecordRetention::Latest,
                    crate::context_plan::ContextRecordBody::text("finish the task"),
                ),
            ));

        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert_eq!(body["input"][1]["role"], "developer");
        assert!(
            body["input"][1]["content"]
                .as_str()
                .is_some_and(|content| content.contains("finish the task"))
        );
        let after = provider.context_prefix(&request).unwrap();
        let observation = after.compare("codex", "codex", "model", "model", &before);
        assert_eq!(observation.reset_reason, None);
        assert_eq!(observation.common_prefix_bytes, before_bytes);
    }

    #[test]
    fn compact_resume_keeps_summary_tail_tool_pair_and_definitions() {
        let provider = CodexProvider::new("codex", "token", "account");
        let turn = crate::event::TurnId::now();
        let request = crate::provider::LlmRequest {
            model: "codex/gpt-test".into(),
            messages: vec![
                crate::message::Message::system_compact_summary(
                    turn.clone(),
                    "retained summary",
                    1,
                    9,
                    9,
                ),
                crate::message::Message::user_text(turn.clone(), "current request"),
                crate::message::Message {
                    role: crate::message::MessageRole::Assistant,
                    parts: vec![crate::message::MessagePart::ToolUse {
                        id: "call_resume".into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({"path": "README.md"}),
                        intent: None,
                    }],
                    turn_id: turn.clone(),
                    origin: crate::message::MessageOrigin::User,
                },
                crate::message::Message {
                    role: crate::message::MessageRole::Tool,
                    parts: vec![crate::message::MessagePart::ToolResult {
                        tool_use_id: "call_resume".into(),
                        content: "contents".into(),
                        is_error: false,
                    }],
                    turn_id: turn,
                    origin: crate::message::MessageOrigin::User,
                },
            ],
            system: Some("stable instructions".into()),
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: true,
            prompt_cache_key: None,
            tools: vec![crate::tool::ToolSpec {
                name: "fs.read".into(),
                description: Some("read a file".into()),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };

        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert_eq!(body["instructions"], "stable instructions");
        assert_eq!(body["input"][0]["content"], "retained summary");
        assert_eq!(body["input"][1]["content"], "current request");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][2]["call_id"], "call_resume");
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["input"][3]["call_id"], "call_resume");
        assert_eq!(body["input"][3]["output"], "contents");
        assert_eq!(body["tools"][0]["name"], "fs_read");
    }

    #[tokio::test]
    async fn streaming_call_acquires_credentials_before_sending_request() {
        let server = MockServer::start().await;
        let responses_url = format!("{}/responses", server.uri());
        let models_url = format!("{}/models", server.uri());
        let (_dir, _hub, provider, access_token) = managed_provider(responses_url, models_url);
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header(
                "authorization",
                format!("Bearer {access_token}"),
            ))
            .and(header("chatgpt-account-id", "account-v2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response-1\",\"model\":\"gpt-test\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let observable = provider.call_streaming(request());
        let message = observable.output.await.unwrap();
        assert_eq!(message.response_id.as_deref(), Some("response-1"));
    }

    #[tokio::test]
    async fn streaming_call_reuses_codex_routing_state_within_a_turn() {
        let server = MockServer::start().await;
        let provider = CodexProvider::new("codex", "token", "account").with_endpoints(
            format!("{}/responses", server.uri()),
            format!("{}/models", server.uri()),
        );
        let mut request = request();
        request.cache_prompt = true;
        request.prompt_cache_key = Some("stable-route".into());
        request.messages.push(crate::message::Message::user_text(
            crate::event::TurnId::now(),
            "hello",
        ));
        let response = || {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header(X_CODEX_TURN_STATE, "sticky-turn")
                .set_body_string(
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response-1\",\"model\":\"gpt-test\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                )
        };
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("session-id", "stable-route"))
            .and(header("thread-id", "stable-route"))
            .and(header("x-client-request-id", "stable-route"))
            .respond_with(response())
            .expect(1)
            .mount(&server)
            .await;

        provider
            .call_streaming(request.clone())
            .output
            .await
            .unwrap();
        server.reset().await;

        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("session-id", "stable-route"))
            .and(header("thread-id", "stable-route"))
            .and(header("x-client-request-id", "stable-route"))
            .and(header(X_CODEX_TURN_STATE, "sticky-turn"))
            .respond_with(response())
            .expect(1)
            .mount(&server)
            .await;

        provider
            .call_streaming(request.clone())
            .output
            .await
            .unwrap();
        server.reset().await;

        request.messages.push(crate::message::Message::user_text(
            crate::event::TurnId::now(),
            "next turn",
        ));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(response())
            .expect(1)
            .mount(&server)
            .await;

        provider.call_streaming(request).output.await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].headers.get(X_CODEX_TURN_STATE).is_none());
    }

    #[tokio::test]
    async fn model_discovery_acquires_credentials_at_poll_time() {
        let server = MockServer::start().await;
        let responses_url = format!("{}/responses", server.uri());
        let models_url = format!("{}/models", server.uri());
        let (_dir, _hub, provider, access_token) = managed_provider(responses_url, models_url);
        mount_models_endpoint(&server, &access_token).await;

        let discovery = provider.try_discover_models();
        let models = discovery.await.unwrap();
        assert_eq!(models[0].slug, "codex/gpt-test");
    }

    #[tokio::test]
    async fn connection_test_acquires_credentials_at_request_time() {
        let server = MockServer::start().await;
        let responses_url = format!("{}/responses", server.uri());
        let models_url = format!("{}/models", server.uri());
        let (_dir, _hub, provider, access_token) = managed_provider(responses_url, models_url);
        mount_models_endpoint(&server, &access_token).await;

        assert_eq!(
            provider.test_connection().await.unwrap(),
            "\"oauth-account\" responded OK"
        );
    }

    #[tokio::test]
    async fn connection_test_handles_multibyte_error_body() {
        let server = MockServer::start().await;
        let responses_url = format!("{}/responses", server.uri());
        let models_url = format!("{}/models", server.uri());
        let (_dir, _hub, provider, access_token) = managed_provider(responses_url, models_url);
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", format!("Bearer {access_token}")))
            .and(header("chatgpt-account-id", "account-v2"))
            .respond_with(ResponseTemplate::new(400).set_body_string("界".repeat(100)))
            .expect(1)
            .mount(&server)
            .await;

        let error = provider.test_connection().await.unwrap_err();
        assert!(error.contains("400"));
        assert!(error.ends_with(&"界".repeat(66)));
    }

    #[tokio::test]
    async fn observable_reads_authoritative_credentials_when_polled() {
        let server = MockServer::start().await;
        let responses_url = format!("{}/responses", server.uri());
        let models_url = format!("{}/models", server.uri());
        let (_dir, hub, provider, _refreshed_token) = managed_provider(responses_url, models_url);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-current"}}"#);
        let current_token = format!("header.{payload}.signature");
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header(
                "authorization",
                format!("Bearer {current_token}"),
            ))
            .and(header("chatgpt-account-id", "account-current"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response-current\",\"model\":\"gpt-test\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let observable = provider.call_streaming(request());
        assert!(
            hub.update_auth_tokens(
                "oauth-account",
                crate::config_hub::AuthTokenUpdate {
                    access_token: current_token,
                    refresh_token: Some("refresh-current".into()),
                    expires_at: chrono::Utc::now().timestamp() + 3_600,
                    account: Some("display-current@example.test".into()),
                },
            )
            .unwrap()
        );

        let message = observable.output.await.unwrap();
        assert_eq!(message.response_id.as_deref(), Some("response-current"));
    }
}
