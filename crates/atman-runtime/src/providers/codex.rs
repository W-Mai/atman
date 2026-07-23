use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable, TurnId};
use crate::message::{ImageData, Message, MessagePart, MessageRole};
use crate::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, StopReason,
    TokenUsage, estimate_tokens,
};
use crate::tool::BoxFut;
use anyhow::Context;

const CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// ChatGPT backend provider. Requires `originator: codex_cli_rs` header for Cloudflare.
pub struct CodexProvider {
    name: String,
    access_token: String,
    account_id: String,
    client: reqwest::Client,
}

impl CodexProvider {
    pub fn new(
        name: impl Into<String>,
        access_token: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            access_token: access_token.into(),
            account_id: account_id.into(),
            client: reqwest::Client::new(),
        }
    }

    fn build_body(&self, req: &LlmRequest) -> ResponsesRequest {
        let model = req
            .model
            .split_once('/')
            .map(|(_, slug)| slug)
            .unwrap_or(&req.model)
            .to_string();

        let input = build_input_items(req);
        let tools = build_tools(&req.tools);

        ResponsesRequest {
            model,
            input,
            instructions: req.system.clone(),
            tools,
            stream: true,
            store: false,
            reasoning: Some(ReasoningConfig {
                effort: Some("medium".into()),
                summary: "auto".into(),
            }),
            text: Some(TextConfig {
                verbosity: "medium".into(),
            }),
            include: Some(vec!["reasoning.encrypted_content".into()]),
        }
    }

    fn build_request(&self, req: &LlmRequest) -> reqwest::RequestBuilder {
        let body = self.build_body(req);
        self.client
            .post(format!("{CODEX_BASE}/responses"))
            .bearer_auth(&self.access_token)
            .header("chatgpt-account-id", &self.account_id)
            .header("originator", "codex_cli_rs")
            .header("OpenAI-Beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .json(&body)
    }
}

fn build_input_items(req: &LlmRequest) -> Vec<InputItem> {
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in &req.messages {
        if m.role == MessageRole::Assistant {
            for p in &m.parts {
                if let MessagePart::ToolUse { id, name, .. } = p {
                    // Codex Responses API requires "_" in function names.
                    tool_names.insert(id.clone(), name.replace('.', "_"));
                }
            }
        }
    }

    let mut items: Vec<InputItem> = Vec::new();

    for m in &req.messages {
        match m.role {
            MessageRole::User => {
                let content = build_user_content(&m.parts);
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
                let (text, tool_calls) = split_assistant_parts(&m.parts);
                if let Some(t) = text {
                    items.push(InputItem {
                        role: Some("assistant".into()),
                        content: Some(t),
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
            MessageRole::System => {}
        }
    }

    items
}

fn build_user_content(parts: &[MessagePart]) -> String {
    let mut parts_out: Vec<serde_json::Value> = Vec::new();
    for p in parts {
        match p {
            MessagePart::Text { text } => {
                parts_out.push(serde_json::json!({"type": "input_text", "text": text}));
            }
            MessagePart::Image { source } => {
                let data = match &source.data {
                    ImageData::Base64 { data } => data.clone(),
                    ImageData::Path { path } => {
                        let bytes = std::fs::read(path).unwrap_or_default();
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    }
                };
                parts_out.push(serde_json::json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", source.media_type, data)
                }));
            }
            MessagePart::CompactSummary { summary, .. } => {
                parts_out.push(serde_json::json!({"type": "input_text", "text": summary}));
            }
            _ => {}
        }
    }
    if parts_out.len() == 1
        && parts_out[0].get("type").and_then(|v| v.as_str()) == Some("input_text")
    {
        parts_out[0]["text"].as_str().unwrap_or("").to_string()
    } else {
        serde_json::to_string(&parts_out).unwrap_or_default()
    }
}

struct AssistantSplit {
    id: String,
    name: String,
    arguments: String,
}

fn split_assistant_parts(parts: &[MessagePart]) -> (Option<String>, Vec<AssistantSplit>) {
    let mut text = String::new();
    let mut tools: Vec<AssistantSplit> = Vec::new();
    for p in parts {
        match p {
            MessagePart::Text { text: t } => text.push_str(t),
            MessagePart::ToolUse { id, name, input } => tools.push(AssistantSplit {
                id: id.clone(),
                // Codex Responses API requires "_" in function names.
                name: name.replace('.', "_"),
                arguments: serde_json::to_string(input).unwrap_or_default(),
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
            // Codex Responses API rejects "." in function names (e.g. "fs.read").
            // Replace with "_" for outbound, revert on inbound.
            name: t.name.replace('.', "_"),
            description: t.description.clone(),
            parameters: t.input_schema.clone(),
        })
        .collect()
}

impl Provider for CodexProvider {
    fn name(&self) -> &str {
        &self.name
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
        let request = self.build_request(&req);
        let turn_id = turn_id_from_req(&req);
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();

        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
            Box::pin(async move {
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
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(RuntimeError::ToolFailed(format!(
                        "codex http {status}: {body}"
                    )));
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
                                // Convert "_" back to "." for atman tool names.
                                slot.name = item["name"].as_str().unwrap_or("").replace('_', ".");
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
                    parts.push(MessagePart::ToolUse {
                        id: tc.id,
                        name: tc.name,
                        input,
                    });
                }

                let token_usage = final_usage.map(|u| TokenUsage {
                    input: u.input_tokens.unwrap_or(0),
                    cached_input: u
                        .input_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0),
                    output: u.output_tokens.unwrap_or(0),
                    cache_write: u
                        .input_tokens_details
                        .as_ref()
                        .and_then(|d| d.cache_write_tokens)
                        .unwrap_or(0),
                    reasoning_tokens: u
                        .output_tokens_details
                        .as_ref()
                        .and_then(|d| d.reasoning_tokens)
                        .unwrap_or(0),
                });

                Ok(AssistantMessage {
                    message: Message {
                        role: MessageRole::Assistant,
                        parts,
                        turn_id,
                    },
                    stop_reason,
                    token_usage: token_usage.unwrap_or_default(),
                    timing: CallTiming::default(),
                    model: resp_model.unwrap_or_default(),
                    response_id: resp_id,
                })
            });
        Observable {
            output,
            events,
            cancel,
        }
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
}

#[derive(Serialize)]
struct InputItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
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
    summary: String,
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

pub struct CodexModel {
    pub slug: String,
    pub context_budget: Option<u64>,
    pub reasoning: bool,
}

/// Fetch available models from the Codex WHAM API.
pub async fn fetch_models(access_token: &str, account_id: &str) -> anyhow::Result<Vec<CodexModel>> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://chatgpt.com/backend-api/wham/models")
        .query(&[("client_version", "0.0.0")])
        .bearer_auth(access_token)
        .header("ChatGPT-Account-Id", account_id)
        .send()
        .await
        .context("fetch codex models from WHAM")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        anyhow::bail!("WHAM models endpoint HTTP {status}: {body}");
    }

    let mut models: Vec<CodexModel> = Vec::new();
    if let Some(list) = body["models"].as_array() {
        for m in list {
            let slug = m["slug"].as_str().unwrap_or("").to_string();
            let slug = slug
                .strip_prefix("codex/")
                .or_else(|| slug.strip_prefix("codex:"))
                .unwrap_or(&slug)
                .to_string();
            let context_budget = m["context_window"].as_u64();
            let reasoning = m["supported_reasoning_levels"]
                .as_array()
                .map(|a: &Vec<serde_json::Value>| !a.is_empty())
                .unwrap_or(false);
            if !slug.is_empty() {
                models.push(CodexModel {
                    slug,
                    context_budget,
                    reasoning,
                });
            }
        }
    }
    Ok(models)
}

/// Fetch WHAM models and register them in the model registry.
pub async fn register_models(access_token: &str, account_id: &str, provider_name: &str) {
    let models = match fetch_models(access_token, account_id).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[atman] fetch codex models failed: {e:#}");
            return;
        }
    };
    let entries: Vec<(String, crate::model_registry::ModelEntry)> = models
        .into_iter()
        .map(|m| {
            let model_name = if m.slug.starts_with("codex-") {
                m.slug.clone()
            } else {
                format!("codex/{name}", name = m.slug)
            };
            let entry = crate::model_registry::ModelEntry {
                model: model_name.clone(),
                provider: Some(provider_name.to_string()),
                context_budget: m.context_budget,
                thinking: Some(m.reasoning),
                ..Default::default()
            };
            (model_name, entry)
        })
        .collect();
    let slugs: Vec<String> = entries.iter().map(|(name, _)| name.clone()).collect();
    crate::model_registry::register_model_entries(entries);
    crate::model_registry::set_discovered_models(slugs.clone());
    eprintln!("[atman] codex models: {}", slugs.join(", "));
}

/// Full bootstrap: refresh token, create CodexProvider, register models.
pub async fn register_from_auth(
    executor: &mut crate::Executor,
    stored: &crate::auth_store::StoredProvider,
) {
    let p = match crate::codex_token::refresh_if_needed(stored).await {
        Ok(Some(refreshed)) => {
            eprintln!("[atman] codex token refreshed for \"{}\"", refreshed.name);
            refreshed
        }
        Ok(None) => stored.clone(),
        Err(e) => {
            eprintln!(
                "[atman] codex token refresh failed for \"{}\": {e:#}",
                stored.name
            );
            stored.clone()
        }
    };
    let account_id = p.account.as_deref().unwrap_or("");
    let provider = std::sync::Arc::new(CodexProvider::new("codex", &p.access_token, account_id));
    executor.providers.register(provider);
    eprintln!(
        "[atman] registered codex provider \"{}\" (account: {})",
        p.name, account_id
    );
    register_models(&p.access_token, account_id, "codex").await;
}
