use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable, TurnId};
use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
use crate::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, ReasoningEffort,
    ReasoningSelection, StopReason, TokenUsage, estimate_tokens,
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
        })
    }

    fn build_request(&self, req: &LlmRequest) -> Result<reqwest::RequestBuilder, RuntimeError> {
        let body = self.build_body(req)?;
        Ok(self
            .client
            .post(format!("{CODEX_BASE}/responses"))
            .bearer_auth(&self.access_token)
            .header("chatgpt-account-id", &self.account_id)
            .header("originator", "codex_cli_rs")
            .header("OpenAI-Beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .json(&body))
    }

    fn validate_reasoning(selection: &ReasoningSelection) -> Result<(), RuntimeError> {
        if matches!(selection, ReasoningSelection::BudgetTokens { .. }) {
            return Err(RuntimeError::ToolFailed(
                "invalid request: Codex Responses does not support token-budget reasoning".into(),
            ));
        }
        Ok(())
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
                    // Codex Responses API requires "_" in function names.
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
                let (text, tool_calls) = split_assistant_parts(&m.parts);
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
                    role: Some("user".into()),
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

fn split_assistant_parts(parts: &[MessagePart]) -> (Option<String>, Vec<AssistantSplit>) {
    let mut text = String::new();
    let mut tools: Vec<AssistantSplit> = Vec::new();
    for p in parts {
        match p {
            MessagePart::Text { text: t } => text.push_str(t),
            MessagePart::ToolUse { id, name, input } => tools.push(AssistantSplit {
                id: id.clone(),
                // Codex Responses API requires "_" in function names.
                name: crate::tool_naming::to_wire(name),
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
            name: crate::tool_naming::to_wire(&t.name),
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
        let preflight =
            Self::validate_reasoning(&req.reasoning).and_then(|()| self.build_request(&req));
        let turn_id = turn_id_from_req(&req);
        let streaming_tools = req.tools.clone();
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();

        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
            Box::pin(async move {
                let request = preflight?;
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
                    parts.push(MessagePart::ToolUse {
                        id: tc.id,
                        name: crate::tool_naming::from_wire(&tc.name, &streaming_tools),
                        input,
                    });
                }

                let token_usage = final_usage.map(|u| {
                    let input_tokens = u.input_tokens.unwrap_or(0);
                    let cached_input = u
                        .input_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0);
                    TokenUsage {
                        // Responses API reports input_tokens as the total input,
                        // including cached tokens. TokenUsage.input is the
                        // uncached portion so the shared window accounting can
                        // add cached_input exactly once.
                        input: normalize_input_tokens(input_tokens, cached_input),
                        cached_input,
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
            });
        Observable {
            output,
            events,
            cancel,
        }
    }

    fn discover_models(
        &self,
    ) -> crate::tool::BoxFut<'static, Vec<crate::provider::DiscoveredModel>> {
        let access_token = self.access_token.clone();
        let account_id = self.account_id.clone();
        Box::pin(async move {
            let client = reqwest::Client::new();
            let resp = match client
                .get("https://chatgpt.com/backend-api/wham/models")
                .query(&[("client_version", "0.0.0")])
                .bearer_auth(&access_token)
                .header("ChatGPT-Account-Id", &account_id)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    crate::notify!(
                        warn,
                        location = Inline,
                        stack = dedupe("codex.models.fetch_failed", 60_000),
                        "fetch codex models failed: {e:#}"
                    );
                    return vec![];
                }
            };
            let Ok(body) = resp.json::<serde_json::Value>().await else {
                return vec![];
            };
            let Some(list) = body["models"].as_array() else {
                return vec![];
            };
            list.iter()
                .filter_map(|m| {
                    let slug = format!("codex/{}", m["slug"].as_str()?);
                    if slug.is_empty() {
                        return None;
                    }
                    let context_budget = m["context_window"].as_u64();
                    let reasoning_efforts = m["supported_reasoning_levels"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|value| value.as_str()?.parse().ok())
                        .collect::<Vec<_>>();
                    let thinking = !reasoning_efforts.is_empty();
                    let input_modalities = m["input_modalities"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|value| match value.as_str()? {
                            "text" => Some(crate::provider::InputModality::Text),
                            "image" => Some(crate::provider::InputModality::Image),
                            "audio" => Some(crate::provider::InputModality::Audio),
                            _ => None,
                        })
                        .collect();
                    let default_reasoning_effort = m["default_reasoning_level"]
                        .as_str()
                        .and_then(|value| value.parse().ok());
                    Some(crate::provider::DiscoveredModel {
                        slug,
                        context_budget,
                        thinking,
                        capabilities: crate::provider::ModelCapabilities {
                            reasoning_efforts,
                            default_reasoning_effort,
                            input_modalities,
                            ..Default::default()
                        },
                    })
                })
                .collect()
        })
    }

    fn test_connection(&self) -> BoxFut<'_, Result<String, String>> {
        let access_token = self.access_token.clone();
        let account_id = self.account_id.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|e| e.to_string())?;
            let resp = client
                .get("https://chatgpt.com/backend-api/wham/models")
                .query(&[("client_version", "0.0.0")])
                .bearer_auth(&access_token)
                .header("ChatGPT-Account-Id", &account_id)
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

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

impl crate::oauth::OAuthProvider for CodexProvider {
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
        let account_id = stored.account.as_deref().unwrap_or("");
        CodexProvider::new(&stored.id, &stored.access_token, account_id)
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

fn normalize_input_tokens(total_input: u64, cached_input: u64) -> u64 {
    total_input.saturating_sub(cached_input)
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
    use super::{CodexProvider, normalize_input_tokens};

    #[test]
    fn input_tokens_exclude_cached_tokens_for_window_accounting() {
        assert_eq!(normalize_input_tokens(100_000, 60_000), 40_000);
    }

    #[test]
    fn cached_tokens_cannot_underflow_input_tokens() {
        assert_eq!(normalize_input_tokens(10, 20), 0);
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
        let request = crate::provider::LlmRequest {
            model: "codex/gpt-test".into(),
            messages: Vec::new(),
            system: None,
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: false,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };
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
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };

        let body = serde_json::to_value(provider.build_body(&request).unwrap()).unwrap();
        assert_eq!(body["instructions"], "stable instructions");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "retained summary");
    }
}
