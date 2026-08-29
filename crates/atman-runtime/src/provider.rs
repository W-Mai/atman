use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
use crate::tool::BoxFut;
use crate::value::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    Persistent,
    Custom(String),
}

impl ReasoningEffort {
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::Persistent => "persistent",
            Self::Custom(value) => value,
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "disabled" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            "ultra" => Ok(Self::Ultra),
            "persistent" => Ok(Self::Persistent),
            "" => Err("reasoning effort must not be empty".into()),
            other => Ok(Self::Custom(other.to_string())),
        }
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReasoningExecutionMode {
    Standard,
    Pro,
    Custom(String),
}

impl ReasoningExecutionMode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Standard => "standard",
            Self::Pro => "pro",
            Self::Custom(value) => value,
        }
    }
}

impl fmt::Display for ReasoningExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningExecutionMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "standard" => Ok(Self::Standard),
            "pro" => Ok(Self::Pro),
            "" => Err("reasoning mode must not be empty".into()),
            other => Ok(Self::Custom(other.to_string())),
        }
    }
}

impl Serialize for ReasoningExecutionMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningExecutionMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSelection {
    #[default]
    ProviderDefault,
    Disabled,
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_mode: Option<ReasoningExecutionMode>,
    },
    Effort {
        effort: ReasoningEffort,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_mode: Option<ReasoningExecutionMode>,
    },
    BudgetTokens {
        tokens: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningWireProfile {
    OpenAiOfficial,
    CompatibleThinking,
    CodexResponses,
    AnthropicMessages,
    Unknown,
}

const OPENAI_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
    ReasoningEffort::Ultra,
];

const CODEX_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
    ReasoningEffort::Ultra,
    ReasoningEffort::Persistent,
];

const ANTHROPIC_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Max,
];

impl ReasoningWireProfile {
    pub fn fallback_efforts(self) -> &'static [ReasoningEffort] {
        match self {
            Self::OpenAiOfficial => OPENAI_REASONING_EFFORTS,
            Self::CodexResponses => CODEX_REASONING_EFFORTS,
            Self::AnthropicMessages => ANTHROPIC_REASONING_EFFORTS,
            Self::CompatibleThinking | Self::Unknown => &[],
        }
    }

    pub fn supports_token_budget(self) -> bool {
        matches!(self, Self::AnthropicMessages)
    }

    pub fn validate(
        self,
        selection: &ReasoningSelection,
        max_tokens: Option<u32>,
    ) -> Result<(), String> {
        if selection.execution_mode().is_some()
            && !matches!(self, Self::CodexResponses | Self::Unknown)
        {
            return Err(match self {
                Self::AnthropicMessages => {
                    "Anthropic does not support reasoning execution mode".into()
                }
                Self::OpenAiOfficial | Self::CompatibleThinking => {
                    "Chat Completions does not support reasoning execution mode".into()
                }
                Self::CodexResponses | Self::Unknown => unreachable!(),
            });
        }

        match (self, selection) {
            (Self::Unknown, _) => Ok(()),
            (
                Self::OpenAiOfficial | Self::CompatibleThinking | Self::CodexResponses,
                ReasoningSelection::BudgetTokens { .. },
            ) => Err(match self {
                Self::CodexResponses => {
                    "Codex Responses does not support token-budget reasoning".into()
                }
                _ => "this OpenAI adapter does not support token-budget reasoning".into(),
            }),
            (
                Self::CompatibleThinking,
                ReasoningSelection::Effort {
                    effort: ReasoningEffort::None,
                    ..
                },
            ) => Ok(()),
            (Self::CompatibleThinking, ReasoningSelection::Effort { effort, .. }) => Err(format!(
                "compatible thinking profile cannot represent effort `{effort}`; use `auto` or select the official OpenAI profile"
            )),
            (
                Self::AnthropicMessages,
                ReasoningSelection::Effort {
                    effort:
                        ReasoningEffort::Minimal
                        | ReasoningEffort::XHigh
                        | ReasoningEffort::Ultra
                        | ReasoningEffort::Persistent,
                    ..
                },
            ) => Err(format!(
                "Anthropic Messages cannot represent effort `{}`; use one of: low, medium, high, max",
                selection.effort().expect("matched effort")
            )),
            (Self::AnthropicMessages, ReasoningSelection::BudgetTokens { tokens })
                if *tokens < 1024 =>
            {
                Err("Anthropic thinking budget must be at least 1024 tokens".into())
            }
            (Self::AnthropicMessages, ReasoningSelection::BudgetTokens { tokens })
                if max_tokens.is_some_and(|max| *tokens >= max) =>
            {
                Err(format!(
                    "Anthropic thinking budget ({tokens}) must be lower than max_tokens ({})",
                    max_tokens.expect("checked max_tokens")
                ))
            }
            _ => Ok(()),
        }
    }
}

impl ReasoningSelection {
    pub fn enabled(&self) -> bool {
        !matches!(self, Self::ProviderDefault | Self::Disabled)
    }

    pub fn effort(&self) -> Option<&ReasoningEffort> {
        match self {
            Self::Effort { effort, .. } => Some(effort),
            _ => None,
        }
    }

    pub fn execution_mode(&self) -> Option<&ReasoningExecutionMode> {
        match self {
            Self::Auto { execution_mode } | Self::Effort { execution_mode, .. } => {
                execution_mode.as_ref()
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for ReasoningSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderDefault => f.write_str("default"),
            Self::Disabled => f.write_str("off"),
            Self::Auto { execution_mode } => {
                f.write_str("auto")?;
                if let Some(mode) = execution_mode {
                    write!(f, "@{mode}")?;
                }
                Ok(())
            }
            Self::Effort {
                effort,
                execution_mode,
            } => {
                effort.fmt(f)?;
                if let Some(mode) = execution_mode {
                    write!(f, "@{mode}")?;
                }
                Ok(())
            }
            Self::BudgetTokens { tokens } => write!(f, "budget:{tokens}"),
        }
    }
}

impl std::str::FromStr for ReasoningSelection {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim().to_ascii_lowercase();
        if matches!(value.as_str(), "default" | "provider_default") {
            return Ok(Self::ProviderDefault);
        }
        if matches!(value.as_str(), "off" | "disabled" | "none") {
            return Ok(Self::Disabled);
        }
        if let Some(tokens) = value.strip_prefix("budget:") {
            let tokens: u32 = tokens
                .parse()
                .map_err(|_| format!("invalid reasoning token budget `{tokens}`"))?;
            if tokens == 0 {
                return Err("reasoning token budget must be positive".into());
            }
            return Ok(Self::BudgetTokens { tokens });
        }
        let (level, execution_mode) = match value.split_once('@') {
            Some((level, mode)) => (level, Some(mode.parse()?)),
            None => (value.as_str(), None),
        };
        if level == "auto" {
            return Ok(Self::Auto { execution_mode });
        }
        Ok(Self::Effort {
            effort: level.parse()?,
            execution_mode,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    #[default]
    Text,
    Image,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    #[default]
    Auto,
    Low,
    High,
    Original,
}

impl ImageDetail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::High => "high",
            Self::Original => "original",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub reasoning_efforts: Vec<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub reasoning_modes: Vec<ReasoningExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_mode: Option<ReasoningExecutionMode>,
    #[serde(default)]
    pub input_modalities: Vec<InputModality>,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub input: Value,
    pub schema: Option<String>,
    pub cache_prompt: bool,
    pub tools: Vec<crate::tool::ToolSpec>,
    pub reasoning: ReasoningSelection,
    /// Seconds without a streaming chunk before the call is cancelled and
    /// retried.  Default 120 s.  0 disables stall detection.
    pub stall_timeout_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cached_input)
            .saturating_add(self.output)
            .saturating_add(self.cache_write)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallTiming {
    pub total_ms: u64,
    pub ttft_ms: Option<u64>,
}

impl CallTiming {
    pub fn tokens_per_second(&self, output_tokens: u64) -> Option<f64> {
        let ttft = self.ttft_ms? as f64;
        let total = self.total_ms as f64;
        let gen_ms = total - ttft;
        if gen_ms <= 0.0 || output_tokens == 0 {
            return None;
        }
        Some(output_tokens as f64 / (gen_ms / 1000.0))
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
    #[allow(dead_code)]
    pub timing: CallTiming,
    pub model: String,
    pub response_id: Option<String>,
}

impl AssistantMessage {
    pub fn text_only(msg: Message) -> Self {
        Self {
            message: msg,
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
            timing: CallTiming::default(),
            model: String::new(),
            response_id: None,
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

    /// Discover available models without capability provenance.
    fn discover_models(&self) -> BoxFut<'static, Vec<DiscoveredModel>> {
        Box::pin(async { vec![] })
    }

    /// Discover available models with capability provenance and typed failures.
    /// The default treats non-empty compatibility results as legacy capability
    /// data and reports an empty result as unsupported.
    fn try_discover_models(
        &self,
    ) -> BoxFut<'static, Result<Vec<DiscoveredModelDetails>, ModelDiscoveryError>> {
        let discovery = self.discover_models();
        Box::pin(async move {
            let models = discovery.await;
            if models.is_empty() {
                return Err(ModelDiscoveryError::Unsupported);
            }
            Ok(models
                .into_iter()
                .map(DiscoveredModelDetails::from)
                .collect())
        })
    }

    fn test_connection(&self) -> BoxFut<'_, Result<String, String>> {
        Box::pin(async { Err("test_connection not implemented".into()) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelDiscoveryError {
    #[error("model discovery is not supported by this provider")]
    Unsupported,
    #[error("model discovery transport failed: {0}")]
    Transport(String),
    #[error("model discovery returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("model discovery returned an invalid response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    pub slug: String,
    pub context_budget: Option<u64>,
    pub thinking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModelDetails {
    pub slug: String,
    pub context_budget: Option<u64>,
    pub capability_knowledge: CapabilityKnowledge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilityKnowledge {
    Legacy { thinking: bool },
    Advertised(ModelCapabilities),
}

impl CapabilityKnowledge {
    pub fn thinking(&self) -> bool {
        match self {
            Self::Legacy { thinking } => *thinking,
            Self::Advertised(capabilities) => {
                capabilities
                    .reasoning_efforts
                    .iter()
                    .chain(capabilities.default_reasoning_effort.iter())
                    .any(|effort| !matches!(effort, ReasoningEffort::None))
                    || !capabilities.reasoning_modes.is_empty()
                    || capabilities.default_reasoning_mode.is_some()
            }
        }
    }

    pub fn advertised(&self) -> Option<&ModelCapabilities> {
        match self {
            Self::Legacy { .. } => None,
            Self::Advertised(capabilities) => Some(capabilities),
        }
    }
}

impl From<DiscoveredModel> for DiscoveredModelDetails {
    fn from(model: DiscoveredModel) -> Self {
        Self {
            slug: model.slug,
            context_budget: model.context_budget,
            capability_knowledge: CapabilityKnowledge::Legacy {
                thinking: model.thinking,
            },
        }
    }
}

impl From<DiscoveredModelDetails> for DiscoveredModel {
    fn from(model: DiscoveredModelDetails) -> Self {
        Self {
            slug: model.slug,
            context_budget: model.context_budget,
            thinking: model.capability_knowledge.thinking(),
        }
    }
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
        origin: MessageOrigin::User,
    }
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: std::sync::Arc<std::sync::RwLock<HashMap<String, Arc<dyn Provider>>>>,
    default: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    lifecycle_owner:
        std::sync::Arc<std::sync::Mutex<Option<crate::provider_lifecycle::ProviderLifecycleOwner>>>,
}

#[derive(Clone)]
pub(crate) struct WeakProviderRegistry {
    providers: std::sync::Weak<std::sync::RwLock<HashMap<String, Arc<dyn Provider>>>>,
    default: std::sync::Weak<std::sync::RwLock<Option<String>>>,
    lifecycle_owner: std::sync::Weak<
        std::sync::Mutex<Option<crate::provider_lifecycle::ProviderLifecycleOwner>>,
    >,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, provider: Arc<dyn Provider>) {
        let name = provider.name().to_string();
        drop(self.register_named(name, provider));
    }

    pub(crate) fn register_named(
        &self,
        name: String,
        provider: Arc<dyn Provider>,
    ) -> Option<Arc<dyn Provider>> {
        let mut providers = self
            .providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut defaults = self
            .default
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if defaults.is_none() {
            *defaults = Some(name.clone());
        }
        providers.insert(name, provider)
    }

    pub fn set_default(&self, name: &str) {
        let providers = self.providers.read().unwrap();
        if providers.contains_key(name) {
            *self.default.write().unwrap() = Some(name.to_string());
        }
    }

    pub fn remove(&self, name: &str) -> bool {
        self.take_named(name).is_some()
    }

    pub(crate) fn take_named(&self, name: &str) -> Option<Arc<dyn Provider>> {
        let mut providers = self.providers.write().unwrap();
        let removed = providers.remove(name);
        if removed.is_some() {
            let mut default = self.default.write().unwrap();
            if default.as_deref() == Some(name) {
                *default = None;
            }
        }
        removed
    }

    pub fn contains(&self, name: &str) -> bool {
        self.providers.read().unwrap().contains_key(name)
    }

    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.providers, &other.providers)
    }

    pub(crate) fn attach_provider_lifecycle(
        &self,
        hub: crate::config_hub::ConfigHub,
    ) -> Option<crate::provider_lifecycle::ProviderLifecycle> {
        let mut owner = self
            .lifecycle_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.is_some() {
            return None;
        }
        let (lifecycle, replaced) =
            crate::provider_lifecycle::ProviderLifecycle::new_deferred(hub, self.clone());
        *owner = Some(lifecycle.owner());
        drop(owner);
        drop(replaced);
        Some(lifecycle)
    }

    pub(crate) fn provider_lifecycle(
        &self,
    ) -> Option<crate::provider_lifecycle::ProviderLifecycle> {
        let owner = self
            .lifecycle_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        Some(crate::provider_lifecycle::ProviderLifecycle::from_owner(
            owner,
            self.clone(),
        ))
    }

    pub(crate) fn downgrade(&self) -> WeakProviderRegistry {
        WeakProviderRegistry {
            providers: Arc::downgrade(&self.providers),
            default: Arc::downgrade(&self.default),
            lifecycle_owner: Arc::downgrade(&self.lifecycle_owner),
        }
    }

    pub fn resolve(&self, model: &str) -> Option<Arc<dyn Provider>> {
        let providers = self.providers.read().unwrap();
        if let Some(p) = providers.get(model) {
            return Some(p.clone());
        }
        if let Some(entry) = crate::model_registry::model_entry(model)
            && let Some(ref provider_name) = entry.provider
        {
            if let Some(provider) = providers.get(provider_name) {
                return Some(provider.clone());
            }
            if !crate::model_registry::is_provider_enabled(provider_name) {
                return None;
            }
            let config_key = format!("config:{provider_name}");
            return providers.get(&config_key).cloned();
        }
        if let Some((prefix, _)) = model.split_once('/')
            && let Some(p) = providers.get(prefix)
        {
            return Some(p.clone());
        }
        None
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.read().unwrap().get(name).cloned()
    }
}

impl WeakProviderRegistry {
    pub(crate) fn upgrade(&self) -> Option<ProviderRegistry> {
        Some(ProviderRegistry {
            providers: self.providers.upgrade()?,
            default: self.default.upgrade()?,
            lifecycle_owner: self.lifecycle_owner.upgrade()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::MockProvider;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct LegacyDiscoveryProvider;

    struct OwnerLockProbeProvider {
        name: String,
        lifecycle_owner:
            Arc<std::sync::Mutex<Option<crate::provider_lifecycle::ProviderLifecycleOwner>>>,
        owner_was_unlocked: Arc<AtomicBool>,
    }

    impl Drop for OwnerLockProbeProvider {
        fn drop(&mut self) {
            self.owner_was_unlocked
                .store(self.lifecycle_owner.try_lock().is_ok(), Ordering::SeqCst);
        }
    }

    impl Provider for OwnerLockProbeProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn call<'a>(
            &'a self,
            _req: LlmRequest,
        ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
            Box::pin(async { unreachable!("not used by owner lock test") })
        }

        fn call_streaming(&self, _req: LlmRequest) -> Observable<AssistantMessage> {
            unreachable!("not used by owner lock test")
        }
    }

    impl Provider for LegacyDiscoveryProvider {
        fn name(&self) -> &str {
            "legacy"
        }

        fn call<'a>(
            &'a self,
            _req: LlmRequest,
        ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
            Box::pin(async { unreachable!("not used by discovery test") })
        }

        fn call_streaming(&self, _req: LlmRequest) -> Observable<AssistantMessage> {
            unreachable!("not used by discovery test")
        }

        fn discover_models(&self) -> BoxFut<'static, Vec<DiscoveredModel>> {
            Box::pin(async {
                vec![DiscoveredModel {
                    slug: "legacy/model".into(),
                    context_budget: Some(8_192),
                    thinking: true,
                }]
            })
        }
    }

    fn fixture_registry() -> ProviderRegistry {
        let reg = ProviderRegistry::new();
        let codex = Arc::new(MockProvider::new("codex"));
        reg.register(codex);
        let openai = Arc::new(MockProvider::new("openai"));
        reg.register(openai);
        reg
    }

    #[tokio::test]
    async fn fallible_discovery_adapts_legacy_provider_implementations() {
        let models = LegacyDiscoveryProvider.try_discover_models().await.unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "legacy/model");
        assert_eq!(models[0].context_budget, Some(8_192));
        assert_eq!(
            models[0].capability_knowledge,
            CapabilityKnowledge::Legacy { thinking: true }
        );
    }

    #[tokio::test]
    async fn fallible_discovery_does_not_treat_missing_legacy_support_as_empty_catalog() {
        let provider = MockProvider::new("mock");

        assert_eq!(
            provider.try_discover_models().await.unwrap_err(),
            ModelDiscoveryError::Unsupported
        );
    }

    #[test]
    fn lifecycle_attach_drops_replaced_providers_after_unlocking_the_owner() {
        const PROVIDER_ID: &str = "owner-lock-provider";

        struct CatalogCleanup;

        impl Drop for CatalogCleanup {
            fn drop(&mut self) {
                crate::model_registry::remove_provider_catalog(PROVIDER_ID);
            }
        }

        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::model_registry::remove_provider_catalog(PROVIDER_ID);
        let _catalog_cleanup = CatalogCleanup;
        let config = tempfile::tempdir().unwrap();
        let hub = crate::config_hub::ConfigHub::from_config_dir(config.path());
        let root =
            crate::provider_lifecycle::ProviderLifecycle::new(hub.clone(), ProviderRegistry::new());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(root.install_pre_discovered_provider(
                crate::auth_store::StoredProvider {
                    id: PROVIDER_ID.into(),
                    name: "Owner Lock".into(),
                    kind: crate::auth_store::ProviderKind::Codex,
                    access_token: "access".into(),
                    refresh_token: None,
                    expires_at: i64::MAX,
                    account: None,
                    enabled: true,
                    model_cache: None,
                },
                Arc::new(MockProvider::new(PROVIDER_ID)),
                vec![DiscoveredModelDetails {
                    slug: "owner-lock-model".into(),
                    context_budget: Some(128_000),
                    capability_knowledge: CapabilityKnowledge::Legacy { thinking: true },
                }],
            ))
            .unwrap();

        let target = ProviderRegistry::new();
        let owner_was_unlocked = Arc::new(AtomicBool::new(false));
        target.register(Arc::new(OwnerLockProbeProvider {
            name: PROVIDER_ID.into(),
            lifecycle_owner: target.lifecycle_owner.clone(),
            owner_was_unlocked: owner_was_unlocked.clone(),
        }));

        let attached = target.attach_provider_lifecycle(hub).unwrap();
        assert!(owner_was_unlocked.load(Ordering::SeqCst));
        drop(attached);
        root.remove_provider(PROVIDER_ID).unwrap();
    }

    #[test]
    fn resolve_prefix_match_codex_slash_model() {
        let reg = fixture_registry();
        let p = reg.resolve("codex/gpt-5.6-terra").expect("should resolve");
        assert_eq!(p.name(), "codex");
    }

    #[test]
    fn resolve_returns_none_for_unknown() {
        let reg = fixture_registry();
        assert!(reg.resolve("some-unknown-model").is_none());
    }

    #[test]
    fn remove_updates_membership_and_clears_only_the_removed_default() {
        let reg = fixture_registry();
        reg.set_default("openai");

        assert!(reg.contains("codex"));
        assert!(reg.contains("openai"));
        assert!(!reg.remove("missing"));
        assert_eq!(reg.default.read().unwrap().as_deref(), Some("openai"));

        assert!(reg.remove("codex"));
        assert!(!reg.contains("codex"));
        assert!(reg.contains("openai"));
        assert_eq!(reg.default.read().unwrap().as_deref(), Some("openai"));

        assert!(reg.remove("openai"));
        assert!(!reg.contains("openai"));
        assert!(reg.default.read().unwrap().is_none());
    }

    #[test]
    fn resolve_model_registry_provider_field_takes_priority() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::model_registry::set_provider_config(Default::default());
        crate::model_registry::register_model_entries(vec![(
            "codex-auto-review".into(),
            crate::model_registry::ModelEntry {
                model: "codex-auto-review".into(),
                provider: Some("codex".into()),
                ..Default::default()
            },
        )]);

        let reg = fixture_registry();
        let p = reg
            .resolve("codex-auto-review")
            .expect("should resolve via model registry provider field");
        assert_eq!(p.name(), "codex");
        crate::model_registry::set_provider_config(Default::default());
    }

    #[test]
    fn resolve_explicit_model_provider_before_slash_prefix() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::model_registry::set_provider_config(Default::default());
        crate::model_registry::register_model_entries(vec![(
            "gateway/model".into(),
            crate::model_registry::ModelEntry {
                model: "api-model".into(),
                provider: Some("target".into()),
                ..Default::default()
            },
        )]);

        let registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider::new("gateway")));
        registry.register(Arc::new(MockProvider::new("config:target")));

        let provider = registry
            .resolve("gateway/model")
            .expect("explicit model provider should resolve");
        assert_eq!(provider.name(), "config:target");

        let registry_without_target = ProviderRegistry::new();
        registry_without_target.register(Arc::new(MockProvider::new("gateway")));
        assert!(registry_without_target.resolve("gateway/model").is_none());
        crate::model_registry::set_provider_config(Default::default());
    }

    #[test]
    fn resolve_exact_live_provider_does_not_depend_on_global_auth_store() {
        const PROVIDER_ID: &str = "exact-live-selected-config-provider";

        struct CatalogCleanup;

        impl Drop for CatalogCleanup {
            fn drop(&mut self) {
                crate::model_registry::remove_provider_catalog(PROVIDER_ID);
            }
        }

        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::model_registry::remove_provider_catalog(PROVIDER_ID);
        let _catalog_cleanup = CatalogCleanup;
        let namespace = "exact-live-selected-config";
        let prepared = crate::model_registry::prepare_provider_catalog(
            crate::model_registry::ProviderDescriptor {
                provider_key: PROVIDER_ID.into(),
                provider_name: "Selected config OAuth".into(),
                namespace: namespace.into(),
                wire_profile: ReasoningWireProfile::CodexResponses,
            },
            &[DiscoveredModelDetails {
                slug: "gpt-selected".into(),
                context_budget: Some(128_000),
                capability_knowledge: CapabilityKnowledge::Advertised(ModelCapabilities::default()),
            }],
        )
        .unwrap();
        crate::model_registry::commit_prepared_provider_catalog(prepared);

        let registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider::new(PROVIDER_ID)));

        let provider = registry
            .resolve(&format!("{namespace}:gpt-selected"))
            .expect("live provider membership should authorize resolution");
        assert_eq!(provider.name(), PROVIDER_ID);
    }

    #[test]
    fn reasoning_selection_string_round_trips() {
        for value in [
            "default",
            "off",
            "auto",
            "auto@pro",
            "minimal",
            "high@standard",
            "xhigh",
            "max",
            "ultra",
            "persistent",
            "budget:4096",
        ] {
            let parsed: ReasoningSelection = value.parse().unwrap();
            assert_eq!(parsed.to_string(), value);
        }
    }

    #[test]
    fn reasoning_selection_rejects_zero_budget() {
        assert!("budget:0".parse::<ReasoningSelection>().is_err());
    }

    #[test]
    fn reasoning_wire_profiles_reject_unrepresentable_controls() {
        let high = ReasoningSelection::Effort {
            effort: ReasoningEffort::High,
            execution_mode: None,
        };
        assert!(
            ReasoningWireProfile::CompatibleThinking
                .validate(&high, None)
                .unwrap_err()
                .contains("cannot represent effort `high`")
        );
        assert!(
            ReasoningWireProfile::CompatibleThinking
                .validate(
                    &ReasoningSelection::Auto {
                        execution_mode: None
                    },
                    None
                )
                .is_ok()
        );
        assert!(
            ReasoningWireProfile::AnthropicMessages
                .validate(
                    &ReasoningSelection::Effort {
                        effort: ReasoningEffort::XHigh,
                        execution_mode: None,
                    },
                    None,
                )
                .unwrap_err()
                .contains("cannot represent effort `xhigh`")
        );
    }
}
