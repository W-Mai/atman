use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable, TurnId};
use crate::message::{
    Message, MessageOrigin, MessagePart, MessageRole, ResponsesRoute, ResponsesState,
};
use crate::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, ReasoningEffort,
    ReasoningSelection, ReasoningWireProfile, StopReason, TokenUsage, estimate_tokens,
};
use crate::tool::BoxFut;

pub(crate) fn build_body(
    req: &LlmRequest,
    route: &ResponsesRoute,
) -> Result<ResponsesRequest, RuntimeError> {
    Ok(ResponsesRequest {
        model: route.model.clone(),
        input: build_input_items(req, route)?,
        instructions: req.system.clone().filter(|value| !value.is_empty()),
        tools: build_tools(&req.tools),
        stream: true,
        store: false,
        reasoning: build_reasoning_config(&req.reasoning),
        text: None,
        include: Some(vec!["reasoning.encrypted_content".into()]),
        prompt_cache_key: req.prompt_cache_key.clone(),
        max_output_tokens: None,
    })
}

pub(crate) fn context_prefix(
    body: &ResponsesRequest,
    req: &LlmRequest,
    profile: crate::context_plan::ContextPrefixProfile,
) -> Result<crate::context_plan::ContextPrefixSnapshot, RuntimeError> {
    use crate::context_plan::{ContextPrefixLane, ContextPrefixSnapshot};
    let mut builder = ContextPrefixSnapshot::builder(profile, req);
    if let Some(instructions) = &body.instructions {
        builder.push(ContextPrefixLane::Stable, instructions)?;
    }
    for tool in &body.tools {
        builder.push(ContextPrefixLane::Tools, tool)?;
    }
    for item in &body.input {
        let lane = if matches!(item, InputItem::Fields(fields) if fields.role.as_deref() == Some("developer"))
        {
            ContextPrefixLane::Records
        } else {
            ContextPrefixLane::Messages
        };
        builder.push(lane, item)?;
    }
    Ok(builder.finish())
}

fn protocol_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("responses protocol: {error}"))
}

/// API-key connection to the Responses endpoint.
pub struct ResponsesProvider {
    discovery: super::openai::OpenAiProvider,
    name: String,
    api_key: String,
    endpoint: String,
    client: reqwest::Client,
    max_tokens: Option<u32>,
    prompt_cache_key: bool,
    max_tools: Option<usize>,
}

impl ResponsesProvider {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let base_url = base_url.into();
        let api_key = api_key.into();
        Self {
            discovery: super::openai::OpenAiProvider::new(&name, &api_key).with_base_url(&base_url),
            name,
            api_key,
            endpoint: format!("{}/responses", base_url.trim_end_matches('/')),
            client: reqwest::Client::new(),
            max_tokens: None,
            prompt_cache_key: true,
            max_tools: None,
        }
    }

    pub fn with_max_tokens(mut self, value: u32) -> Self {
        self.max_tokens = Some(value);
        self
    }

    pub(crate) fn with_client(mut self, client: reqwest::Client) -> Self {
        self.discovery = self.discovery.with_client(client.clone());
        self.client = client;
        self
    }

    pub fn with_prompt_cache_key(mut self, enabled: bool) -> Self {
        self.prompt_cache_key = enabled;
        self
    }

    pub fn with_max_tools(mut self, value: Option<usize>) -> Self {
        self.max_tools = value;
        self
    }

    fn route(&self, req: &LlmRequest) -> ResponsesRoute {
        ResponsesRoute {
            provider: self.name.clone(),
            endpoint: self.endpoint.clone(),
            model: req.model.clone(),
        }
    }

    fn build_body(&self, req: &LlmRequest) -> Result<ResponsesRequest, RuntimeError> {
        ReasoningWireProfile::OpenAiResponses
            .validate(&req.reasoning, self.max_tokens)
            .map_err(|error| RuntimeError::ToolFailed(format!("invalid request: {error}")))?;
        let mut body = build_body(req, &self.route(req))?;
        body.max_output_tokens = self.max_tokens;
        if !self.prompt_cache_key {
            body.prompt_cache_key = None;
        }
        Ok(body)
    }
}

impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            prompt_cache_key: self.prompt_cache_key,
            context_prefix_profile: crate::context_plan::ContextPrefixProfile::OpenAiResponses,
            max_tools: self.max_tools,
        }
    }

    fn context_prefix(
        &self,
        req: &LlmRequest,
    ) -> Result<crate::context_plan::ContextPrefixSnapshot, RuntimeError> {
        context_prefix(
            &self.build_body(req)?,
            req,
            crate::context_plan::ContextPrefixProfile::OpenAiResponses,
        )
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        let observable = self.call_streaming(req);
        Box::pin(async move {
            let _events = observable.events;
            observable.output.await
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let body = self.build_body(&req);
        let route = self.route(&req);
        let tools = req.tools.clone();
        let turn_id = turn_id_from_req(&req);
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let output = Box::pin(async move {
            let body = body?;
            let response = tokio::select! {
                biased;
                _ = task_cancel.cancelled() => return Err(RuntimeError::Cancelled("responses cancelled before send".into())),
                response = client.post(&route.endpoint).bearer_auth(api_key).header("accept", "text/event-stream").json(&body).send() => {
                    response.map_err(|error| RuntimeError::ToolFailed(format!("responses net: {error}")))?
                }
            };
            let status = response.status();
            if !status.is_success() {
                let body = tokio::select! {
                    biased;
                    _ = task_cancel.cancelled() => return Err(RuntimeError::Cancelled("responses cancelled while reading error".into())),
                    body = response.text() => body.unwrap_or_default(),
                };
                if let Some(reason) = super::classify_attachment_error(status.as_u16(), &body) {
                    return Err(RuntimeError::AttachmentError { reason });
                }
                return Err(RuntimeError::ToolFailed(format!(
                    "responses http {status}: {body}"
                )));
            }
            read_stream(response, route, tools, turn_id, tx, task_cancel).await
        });
        Observable {
            output,
            events,
            cancel,
        }
    }

    fn discover_models(&self) -> BoxFut<'static, Vec<crate::provider::DiscoveredModel>> {
        self.discovery.discover_models()
    }

    fn try_discover_models(
        &self,
    ) -> BoxFut<
        'static,
        Result<Vec<crate::provider::DiscoveredModelDetails>, crate::provider::ModelDiscoveryError>,
    > {
        self.discovery.try_discover_models()
    }

    fn test_connection(&self) -> BoxFut<'_, Result<String, String>> {
        self.discovery.test_connection()
    }
}

pub(crate) fn turn_id_from_req(req: &LlmRequest) -> TurnId {
    req.messages
        .first()
        .map(|message| message.turn_id.clone())
        .unwrap_or_else(TurnId::now)
}

/// Reduces a stream only after a successful terminal response; partial tool calls never escape.
pub(crate) async fn read_stream(
    response: reqwest::Response,
    route: ResponsesRoute,
    tools: Vec<crate::tool::ToolSpec>,
    turn_id: TurnId,
    tx: broadcast::Sender<NodeEvent>,
    cancel: CancellationToken,
) -> Result<AssistantMessage, RuntimeError> {
    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    let mut stream = response.bytes_stream().eventsource();
    let mut reducer = ResponseReducer::default();
    while let Some(event) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(RuntimeError::Cancelled("responses cancelled mid-stream".into())),
        next = stream.next() => next,
    } {
        let event = event.map_err(|error| match error {
            eventsource_stream::EventStreamError::Transport(error) => {
                RuntimeError::ToolFailed(format!("responses network: {error}"))
            }
            error => protocol_error(format!("sse: {error}")),
        })?;
        if event.data.is_empty() || event.data == "[DONE]" {
            continue;
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&event.data).map_err(protocol_error)?;
        if reducer.push(parsed, &tools, &tx)? {
            break;
        }
    }
    let output = reducer.finish(route, tools, turn_id)?;
    let _ = tx.send(NodeEvent::LlmDone {
        total_tokens: output.token_usage.output,
    });
    Ok(output)
}

#[derive(Default)]
struct ResponseReducer {
    items: std::collections::BTreeMap<u64, serde_json::Value>,
    completed_items: std::collections::BTreeSet<u64>,
    text: String,
    thinking: String,
    estimated_output: u64,
    terminal: Option<serde_json::Value>,
}

impl ResponseReducer {
    fn push(
        &mut self,
        event: serde_json::Value,
        tools: &[crate::tool::ToolSpec],
        tx: &broadcast::Sender<NodeEvent>,
    ) -> Result<bool, RuntimeError> {
        if matches!(
            event["type"].as_str(),
            Some(
                "response.output_item.added"
                    | "response.output_item.done"
                    | "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done"
            )
        ) && event["output_index"]
            .as_u64()
            .is_some_and(|index| self.completed_items.contains(&index))
        {
            return Err(protocol_error("completed output item was modified"));
        }
        match event["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.text.push_str(delta);
                    self.estimated_output += estimate_tokens(delta);
                    let _ = tx.send(NodeEvent::LlmChunk {
                        text: delta.into(),
                        cumulative_tokens: self.estimated_output,
                    });
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.thinking.push_str(delta);
                    let _ = tx.send(NodeEvent::ThinkingChunk { text: delta.into() });
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| protocol_error("missing output index"))?;
                let item = event
                    .get("item")
                    .filter(|item| item.is_object())
                    .ok_or_else(|| protocol_error("missing output item"))?;
                if event["type"] == "response.output_item.done" {
                    self.completed_items.insert(index);
                }
                self.items.insert(index, item.clone());
            }
            "response.function_call_arguments.delta" => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| protocol_error("missing output index"))?;
                let item = self
                    .items
                    .get_mut(&index)
                    .ok_or_else(|| protocol_error("function arguments precede output item"))?;
                let delta = event["delta"]
                    .as_str()
                    .ok_or_else(|| protocol_error("missing argument delta"))?;
                let arguments = item["arguments"].as_str().unwrap_or_default().to_owned() + delta;
                item["arguments"] = arguments.into();
                let _ = tx.send(NodeEvent::ToolCallDraft {
                    index: usize::try_from(index).map_err(protocol_error)?,
                    call_id: item["call_id"].as_str().unwrap_or_default().into(),
                    name: crate::tool_naming::from_wire(
                        item["name"].as_str().unwrap_or_default(),
                        tools,
                    ),
                    arguments_delta: delta.into(),
                });
            }
            "response.function_call_arguments.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| protocol_error("missing output index"))?;
                let item = self
                    .items
                    .get_mut(&index)
                    .ok_or_else(|| protocol_error("function arguments precede output item"))?;
                item["arguments"] = event["arguments"].clone();
            }
            "response.completed" | "response.incomplete" => {
                let response = event
                    .get("response")
                    .ok_or_else(|| protocol_error("missing terminal response"))?;
                let status = response["status"].as_str().unwrap_or_default();
                if !matches!(status, "completed" | "incomplete") {
                    return Err(protocol_error(format!(
                        "unexpected terminal status: {status}"
                    )));
                }
                if let Some(output) = response["output"].as_array() {
                    if self
                        .completed_items
                        .iter()
                        .any(|index| *index >= output.len() as u64)
                    {
                        return Err(protocol_error("terminal response omitted a completed item"));
                    }
                    let mut items = std::collections::BTreeMap::new();
                    for (index, item) in output.iter().enumerate() {
                        let index = index as u64;
                        let item = if self.completed_items.contains(&index) {
                            let completed = &self.items[&index];
                            if completed["id"] != item["id"] || completed["type"] != item["type"] {
                                return Err(protocol_error(
                                    "terminal output item identity changed",
                                ));
                            }
                            // Item completion owns replay state; the response summary may omit it.
                            completed
                        } else {
                            item
                        };
                        items.insert(index, item.clone());
                    }
                    self.items = items;
                } else if self.items.is_empty()
                    || self
                        .items
                        .keys()
                        .any(|index| !self.completed_items.contains(index))
                {
                    return Err(protocol_error("terminal response has no completed output"));
                }
                self.terminal = Some(response.clone());
                return Ok(true);
            }
            "error" | "response.failed" => {
                let error = event
                    .get("error")
                    .or_else(|| event.pointer("/response/error"))
                    .unwrap_or(&event);
                return Err(protocol_error(
                    error["message"].as_str().unwrap_or("response failed"),
                ));
            }
            _ => {}
        }
        Ok(false)
    }

    fn finish(
        self,
        route: ResponsesRoute,
        tools: Vec<crate::tool::ToolSpec>,
        turn_id: TurnId,
    ) -> Result<AssistantMessage, RuntimeError> {
        let terminal = self
            .terminal
            .ok_or_else(|| protocol_error("stream ended before a terminal response"))?;
        let incomplete = terminal["status"].as_str() == Some("incomplete");
        let mut parts = Vec::new();
        let mut has_text = false;
        let mut has_thinking = false;
        let mut calls = std::collections::HashSet::new();
        for item in self.items.into_values() {
            match item["type"].as_str().unwrap_or_default() {
                "reasoning" => {
                    let state: crate::message::ResponsesReasoningItem =
                        serde_json::from_value(item).map_err(protocol_error)?;
                    if state.id.is_empty() {
                        return Err(protocol_error("reasoning item has no id"));
                    }
                    let summary = state
                        .summary
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>();
                    if !summary.is_empty() {
                        has_thinking = true;
                        parts.push(MessagePart::Thinking {
                            thinking: summary,
                            signature: None,
                        });
                    }
                    parts.push(MessagePart::ResponsesState {
                        route: route.clone(),
                        state: ResponsesState::Reasoning(state),
                    });
                }
                "message" => {
                    parts.push(MessagePart::ResponsesState {
                        route: route.clone(),
                        state: ResponsesState::Message {
                            id: item["id"].as_str().map(str::to_owned),
                            phase: item["phase"].as_str().map(str::to_owned),
                        },
                    });
                    if let Some(content) = item["content"].as_array() {
                        let mut message_text = String::new();
                        for part in content {
                            let text = match part["type"].as_str() {
                                Some("output_text") => part["text"].as_str(),
                                Some("refusal") => part["refusal"].as_str(),
                                _ => None,
                            };
                            if let Some(text) = text.filter(|text| !text.is_empty()) {
                                has_text = true;
                                message_text.push_str(text);
                            }
                        }
                        if !message_text.is_empty() {
                            parts.push(MessagePart::Text { text: message_text });
                        }
                    }
                }
                "function_call" => {
                    if incomplete {
                        return Err(protocol_error("incomplete function call response"));
                    }
                    if item["status"]
                        .as_str()
                        .is_some_and(|status| status != "completed")
                    {
                        return Err(protocol_error("function call item is not completed"));
                    }
                    let id = item["call_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| protocol_error("function call has no call_id"))?;
                    if !calls.insert(id.to_owned()) {
                        return Err(protocol_error("duplicate function call id"));
                    }
                    let wire_name = item["name"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| protocol_error("function call has no name"))?;
                    let arguments = item["arguments"]
                        .as_str()
                        .ok_or_else(|| protocol_error("function call has no arguments"))?;
                    let input: serde_json::Value =
                        serde_json::from_str(arguments).map_err(protocol_error)?;
                    if !input.is_object() {
                        return Err(protocol_error("function arguments must be an object"));
                    }
                    let name = crate::tool_naming::from_wire(wire_name, &tools);
                    let (input, intent) =
                        crate::message::decode_tool_call_input(input, &name, &tools);
                    if let Some(item_id) = item["id"].as_str() {
                        parts.push(MessagePart::ResponsesState {
                            route: route.clone(),
                            state: ResponsesState::FunctionCall {
                                id: item_id.into(),
                                call_id: id.into(),
                            },
                        });
                    }
                    parts.push(MessagePart::ToolUse {
                        id: id.into(),
                        name,
                        input,
                        intent,
                    });
                }
                _ => {}
            }
        }
        if !has_thinking && !self.thinking.is_empty() {
            parts.insert(
                0,
                MessagePart::Thinking {
                    thinking: self.thinking,
                    signature: None,
                },
            );
        }
        if !has_text && !self.text.is_empty() {
            parts.push(MessagePart::Text { text: self.text });
        }
        let token_usage = terminal
            .get("usage")
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value::<ResponsesUsage>(value.clone()).map_err(protocol_error)
            })
            .transpose()?
            .map(TokenUsage::from)
            .unwrap_or(TokenUsage {
                output: self.estimated_output,
                ..Default::default()
            });
        Ok(AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts,
                turn_id,
                origin: MessageOrigin::User,
            },
            stop_reason: if incomplete {
                StopReason::Length
            } else if !calls.is_empty() {
                StopReason::ToolUse
            } else {
                StopReason::End
            },
            token_usage,
            timing: CallTiming::default(),
            model: terminal["model"].as_str().unwrap_or(&route.model).into(),
            response_id: terminal["id"].as_str().map(str::to_owned),
        })
    }
}

impl From<ResponsesUsage> for TokenUsage {
    fn from(usage: ResponsesUsage) -> Self {
        let cached_input = usage
            .input_tokens_details
            .as_ref()
            .and_then(|value| value.cached_tokens)
            .unwrap_or_default();
        let cache_write = usage
            .input_tokens_details
            .as_ref()
            .and_then(|value| value.cache_write_tokens)
            .unwrap_or_default();
        Self {
            input: crate::provider::regular_input_tokens(
                usage.input_tokens.unwrap_or_default(),
                cached_input,
                cache_write,
            ),
            cached_input,
            cache_write,
            output: usage.output_tokens.unwrap_or_default(),
            reasoning_tokens: usage
                .output_tokens_details
                .and_then(|value| value.reasoning_tokens)
                .unwrap_or_default(),
        }
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

fn build_input_items(
    req: &LlmRequest,
    route: &ResponsesRoute,
) -> Result<Vec<InputItem>, RuntimeError> {
    let mut items: Vec<InputItem> = Vec::new();

    for m in &req.messages {
        match m.role {
            MessageRole::User => {
                let Some(content) = build_user_content(&m.parts)? else {
                    continue;
                };
                items.push(InputItem::Fields(InputFields {
                    id: None,
                    phase: None,
                    role: Some("user".into()),
                    content: Some(content),
                    item_type: Some("message".into()),
                    call_id: None,
                    name: None,
                    arguments: None,
                    output: None,
                }));
            }
            MessageRole::Assistant => {
                let mut message_metadata: Option<(Option<String>, Option<String>)> = None;
                let mut call_metadata = std::collections::HashMap::new();
                for part in &m.parts {
                    if let MessagePart::ResponsesState {
                        route: source,
                        state,
                    } = part
                    {
                        message_metadata = None;
                        if source == route {
                            match state {
                                ResponsesState::Reasoning(item) => {
                                    let mut raw =
                                        serde_json::to_value(item).map_err(protocol_error)?;
                                    raw["type"] = "reasoning".into();
                                    items.push(InputItem::Opaque(raw));
                                }
                                ResponsesState::Message { id, phase } => {
                                    message_metadata = Some((id.clone(), phase.clone()))
                                }
                                ResponsesState::FunctionCall { id, call_id } => {
                                    call_metadata.insert(call_id.clone(), id.clone());
                                }
                            }
                        }
                        continue;
                    }
                    let (text, tool_calls) =
                        split_assistant_parts(std::slice::from_ref(part), &req.tools);
                    if let Some(text) = text {
                        let (id, phase) = message_metadata.take().unwrap_or_default();
                        items.push(InputItem::Fields(InputFields {
                            role: Some("assistant".into()),
                            content: Some(InputContent::Output(vec![ResponseOutputText {
                                text,
                                annotations: Vec::new(),
                                kind: "output_text",
                            }])),
                            item_type: Some("message".into()),
                            call_id: None,
                            name: None,
                            arguments: None,
                            output: None,
                            id,
                            phase,
                        }));
                    }
                    for tc in tool_calls {
                        items.push(InputItem::Fields(InputFields {
                            role: None,
                            content: None,
                            item_type: Some("function_call".into()),
                            id: call_metadata.remove(&tc.id),
                            phase: None,
                            call_id: Some(tc.id),
                            name: Some(tc.name),
                            arguments: Some(tc.arguments),
                            output: None,
                        }));
                    }
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
                        items.push(InputItem::Fields(InputFields {
                            role: None,
                            content: None,
                            item_type: Some("function_call_output".into()),
                            id: None,
                            phase: None,
                            call_id: Some(tool_use_id.clone()),
                            name: None,
                            arguments: None,
                            output: Some(content.clone()),
                        }));
                    }
                }
            }
            MessageRole::System => {
                let Some(content) = build_user_content(&m.parts)? else {
                    continue;
                };
                items.push(InputItem::Fields(InputFields {
                    id: None,
                    phase: None,
                    role: Some(
                        if m.origin == MessageOrigin::Internal
                            && m.parts
                                .iter()
                                .any(|part| matches!(part, MessagePart::ContextRecord(_)))
                        {
                            "developer"
                        } else if m
                            .parts
                            .iter()
                            .any(|part| matches!(part, MessagePart::CompactSummary { .. }))
                        {
                            "user"
                        } else {
                            "system"
                        }
                        .into(),
                    ),
                    content: Some(content),
                    item_type: Some("message".into()),
                    call_id: None,
                    name: None,
                    arguments: None,
                    output: None,
                }));
            }
        }
    }

    Ok(items)
}

fn build_user_content(parts: &[MessagePart]) -> Result<Option<InputContent>, RuntimeError> {
    let mut parts_out: Vec<ResponseInputContent> = Vec::new();
    for p in parts {
        match p {
            MessagePart::ContextRecord(record) => {
                let text = record.render_for_model();
                if !text.is_empty() {
                    parts_out.push(ResponseInputContent::InputText { text });
                }
            }
            MessagePart::Text { text } => {
                if !text.is_empty() {
                    parts_out.push(ResponseInputContent::InputText { text: text.clone() });
                }
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
                if !summary.is_empty() {
                    parts_out.push(ResponseInputContent::InputText {
                        text: summary.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    if let [ResponseInputContent::InputText { text }] = parts_out.as_slice() {
        Ok(Some(InputContent::Text(text.clone())))
    } else if parts_out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(InputContent::Parts(parts_out)))
    }
}

pub(crate) struct AssistantSplit {
    id: String,
    name: String,
    pub(crate) arguments: String,
}

pub(crate) fn split_assistant_parts(
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
            strict: false,
        })
        .collect()
}

#[derive(Serialize)]
pub(crate) struct ResponsesRequest {
    pub(crate) model: String,
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
    pub(crate) text: Option<TextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum InputItem {
    Fields(InputFields),
    Opaque(serde_json::Value),
}

#[derive(Serialize)]
struct InputFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
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
    Output(Vec<ResponseOutputText>),
}

#[derive(Serialize)]
struct ResponseOutputText {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    annotations: Vec<serde_json::Value>,
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
    strict: bool,
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
pub(crate) struct TextConfig {
    pub(crate) verbosity: String,
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
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request() -> LlmRequest {
        LlmRequest {
            model: "vendor/reasoning-model".into(),
            messages: vec![Message::user_text(TurnId::now(), "inspect")],
            system: Some("stable instructions".into()),
            input: crate::value::Value::Unit,
            schema: None,
            cache_prompt: true,
            prompt_cache_key: Some("routing-key".into()),
            tools: vec![crate::tool::ToolSpec {
                name: "probe.read".into(),
                description: Some("Read a value".into()),
                input_schema: json!({"type":"object","properties":{"value":{"type":"integer"}},"required":[]}),
            }],
            reasoning: ReasoningSelection::Effort {
                effort: ReasoningEffort::High,
                execution_mode: None,
            },
            stall_timeout_secs: 0,
        }
    }

    fn output() -> serde_json::Value {
        json!([
            {"type":"reasoning","id":"rs-1","summary":[{"type":"summary_text","text":"Inspecting"}],"encrypted_content":"opaque-reasoning","status":"completed"},
            {"type":"message","id":"msg-1","phase":"commentary","role":"assistant","content":[{"type":"output_text","text":"Reading now"}]},
            {"type":"function_call","id":"fc-1","call_id":"call-1","name":crate::tool_naming::to_wire("probe.read"),"arguments":"{\"value\":7}"}
        ])
    }

    fn completed(output: serde_json::Value) -> serde_json::Value {
        json!({"type":"response.completed","response":{"id":"resp-1","model":"vendor/reasoning-model","status":"completed","output":output,"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":60},"output_tokens":12,"output_tokens_details":{"reasoning_tokens":4}}}})
    }

    fn reduce(
        event: serde_json::Value,
        route: ResponsesRoute,
    ) -> Result<AssistantMessage, RuntimeError> {
        let (tx, _) = broadcast::channel(32);
        let req = request();
        let mut reducer = ResponseReducer::default();
        reducer.push(event, &req.tools, &tx)?;
        reducer.finish(route, req.tools, TurnId::now())
    }

    #[tokio::test]
    async fn api_key_round_trip_preserves_protocol_state_without_codex_headers() {
        let server = MockServer::start().await;
        let first = format!("data: {}\n\n", completed(output()));
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(first),
            )
            .mount(&server)
            .await;
        let provider = ResponsesProvider::new("gateway", format!("{}/v1", server.uri()), "api-key")
            .with_max_tokens(512);
        let mut req = request();
        let first = provider.call(req.clone()).await.unwrap();
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        assert_eq!(first.token_usage.input, 40);
        assert_eq!(first.token_usage.cached_input, 60);
        assert_eq!(first.token_usage.reasoning_tokens, 4);
        assert!(!first.message.text_concat().contains("opaque-reasoning"));

        // Persisted messages are the sole source of the next stateless request.
        let restored =
            serde_json::from_slice(&serde_json::to_vec(&first.message).unwrap()).unwrap();
        req.messages.push(restored);
        req.messages.push(Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "call-1".into(),
                content: "7".into(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        });
        provider.call(req.clone()).await.unwrap();
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2);
        for request in &received {
            assert_eq!(
                request.headers.get("authorization").unwrap(),
                "Bearer api-key"
            );
            for header in [
                "originator",
                "chatgpt-account-id",
                "OpenAI-Beta",
                "x-codex-turn-state",
            ] {
                assert!(request.headers.get(header).is_none());
            }
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["model"], "vendor/reasoning-model");
            assert_eq!(body["store"], false);
            assert_eq!(body["tools"][0]["strict"], false);
            assert_eq!(body["reasoning"]["effort"], "high");
            assert!(body.get("reasoning_effort").is_none());
            assert!(body.get("thinking").is_none());
            assert_eq!(body["max_output_tokens"], 512);
            assert!(body.get("previous_response_id").is_none());
        }
        let body: serde_json::Value = received[1].body_json().unwrap();
        assert_eq!(body["input"][1]["encrypted_content"], "opaque-reasoning");
        assert_eq!(body["input"][2]["id"], "msg-1");
        assert_eq!(body["input"][2]["phase"], "commentary");
        assert_eq!(body["input"][3]["id"], "fc-1");
        assert_eq!(body["input"][3]["call_id"], "call-1");
        assert_eq!(body["input"][4]["type"], "function_call_output");
        assert_eq!(body["input"][4]["call_id"], "call-1");

        let mut other = req.clone();
        other.model = "vendor/other-model".into();
        let changed = serde_json::to_value(provider.build_body(&other).unwrap()).unwrap();
        assert!(!changed.to_string().contains("opaque-reasoning"));
        assert!(changed["input"][1].get("phase").is_none());
        let other_connection =
            ResponsesProvider::new("other", format!("{}/v1", server.uri()), "api-key");
        let changed = serde_json::to_value(other_connection.build_body(&req).unwrap()).unwrap();
        assert!(!changed.to_string().contains("opaque-reasoning"));
    }

    #[test]
    fn interleaved_arguments_require_a_valid_terminal_response() {
        let req = request();
        let route = ResponsesRoute {
            provider: "test".into(),
            endpoint: "https://gateway.example/v1/responses".into(),
            model: req.model.clone(),
        };
        let (tx, _) = broadcast::channel(32);
        let mut reducer = ResponseReducer::default();
        for (index, id) in [(1, "call-a"), (3, "call-b")] {
            reducer.push(json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","call_id":id,"name":"probe","arguments":""}}), &req.tools, &tx).unwrap();
        }
        for (index, delta) in [(3, "{\"n\":"), (1, "{\"n\":1}"), (3, "2}")] {
            reducer.push(json!({"type":"response.function_call_arguments.delta","output_index":index,"delta":delta}), &req.tools, &tx).unwrap();
        }
        for (index, item) in reducer.items.clone() {
            reducer
                .push(
                    json!({"type":"response.output_item.done","output_index":index,"item":item}),
                    &req.tools,
                    &tx,
                )
                .unwrap();
        }
        reducer
            .push(
                json!({"type":"response.completed","response":{"status":"completed"}}),
                &req.tools,
                &tx,
            )
            .unwrap();
        let result = reducer
            .finish(route.clone(), req.tools.clone(), TurnId::now())
            .unwrap();
        let calls: Vec<_> = result
            .message
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::ToolUse { id, input, .. } => {
                    Some((id.as_str(), input["n"].as_i64().unwrap()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(calls, [("call-a", 1), ("call-b", 2)]);
        assert!(
            ResponseReducer::default()
                .finish(route.clone(), vec![], TurnId::now())
                .is_err()
        );
        let malformed =
            json!([{"type":"function_call","call_id":"call-a","name":"probe","arguments":"{"}]);
        assert!(reduce(completed(malformed), route.clone()).is_err());
        let mut incomplete = completed(output());
        incomplete["type"] = "response.incomplete".into();
        incomplete["response"]["status"] = "incomplete".into();
        assert!(reduce(incomplete, route.clone()).is_err());
        assert!(
            reduce(
                json!({"type":"response.failed","response":{"error":{"message":"failed"}}}),
                route
            )
            .is_err()
        );
    }

    #[test]
    fn completed_items_own_replay_state_across_terminal_summaries() {
        let req = request();
        let route = ResponsesRoute {
            provider: "test".into(),
            endpoint: "https://gateway.example/v1/responses".into(),
            model: req.model.clone(),
        };
        let (tx, _) = broadcast::channel(32);
        let mut reducer = ResponseReducer::default();
        let complete = output();
        for item in [&complete[0], &complete[2]] {
            let mut unfinished = ResponseReducer::default();
            unfinished
                .push(
                    json!({"type":"response.output_item.added","output_index":0,"item":item}),
                    &req.tools,
                    &tx,
                )
                .unwrap();
            assert!(
                unfinished
                    .push(
                        json!({"type":"response.completed","response":{"status":"completed"}}),
                        &req.tools,
                        &tx
                    )
                    .is_err()
            );
        }
        let mut summary = complete.clone();
        summary[0]
            .as_object_mut()
            .unwrap()
            .remove("encrypted_content");
        for (index, item) in complete.as_array().unwrap().iter().enumerate() {
            reducer
                .push(
                    json!({"type":"response.output_item.done","output_index":index,"item":item}),
                    &req.tools,
                    &tx,
                )
                .unwrap();
        }
        reducer.push(completed(summary), &req.tools, &tx).unwrap();
        let response = reducer
            .finish(route.clone(), req.tools.clone(), TurnId::now())
            .unwrap();
        let mut next = req.clone();
        next.messages
            .push(serde_json::from_value(serde_json::to_value(response.message).unwrap()).unwrap());
        let body = serde_json::to_value(build_body(&next, &route).unwrap()).unwrap();
        assert_eq!(body["input"][1]["encrypted_content"], "opaque-reasoning");

        for terminal_output in [json!([]), json!([{"type":"reasoning","id":"other"}])] {
            let mut reducer = ResponseReducer::default();
            reducer
                .push(
                    json!({"type":"response.output_item.done","output_index":0,"item":complete[0]}),
                    &req.tools,
                    &tx,
                )
                .unwrap();
            assert!(
                reducer
                    .push(completed(terminal_output), &req.tools, &tx)
                    .is_err()
            );
        }
        let mut finalized = ResponseReducer::default();
        finalized
            .push(
                json!({"type":"response.output_item.done","output_index":0,"item":complete[2]}),
                &req.tools,
                &tx,
            )
            .unwrap();
        assert!(finalized.push(json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}), &req.tools, &tx).is_err());
    }

    #[test]
    fn pruning_incomplete_tool_pairs_drops_opaque_state() {
        let req = request();
        let route = ResponsesRoute {
            provider: "test".into(),
            endpoint: "https://gateway.example/v1/responses".into(),
            model: req.model.clone(),
        };
        let response = reduce(completed(output()), route).unwrap();
        let mut messages = vec![response.message];
        crate::message::retain_complete_tool_pairs(&mut messages);
        assert!(!messages[0].parts.iter().any(|part| matches!(
            part,
            MessagePart::ResponsesState { .. } | MessagePart::ToolUse { .. }
        )));
        assert_eq!(messages[0].text_concat(), "Reading now");
    }
}
