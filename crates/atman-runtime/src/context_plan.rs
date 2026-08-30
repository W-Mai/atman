use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::provider::LlmRequest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ContextPlanId(pub Uuid);

impl ContextPlanId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for ContextPlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Provider-neutral identity around one compiled model request.
///
/// The wrapper does not alter the request. Later context compiler stages add
/// token lanes, epochs, cache metadata, and context records alongside it.
#[derive(Debug, Clone)]
pub struct ModelContextPlan {
    id: ContextPlanId,
    request: LlmRequest,
    token_lanes: ContextTokenLanes,
    call_purpose: ContextCallPurpose,
    call_identity: ContextCallIdentity,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContextPrefixProfile {
    ProviderNeutral,
    OpenAiChat,
    AnthropicMessages,
    CodexResponses,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContextPrefixLane {
    Stable,
    Tools,
    Messages,
    Records,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextCacheResetReason {
    ColdStart,
    CacheDisabled,
    CacheEnabled,
    ProviderChanged,
    ModelChanged,
    ProjectionChanged,
    StableChanged,
    ToolsChanged,
    Compaction,
    MessagePrefixChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextCacheObservation {
    pub profile: ContextPrefixProfile,
    pub wire_prefix_digest: String,
    pub wire_prefix_bytes: u64,
    pub wire_prefix_tokens: u64,
    pub common_prefix_bytes: u64,
    pub common_prefix_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_reason: Option<ContextCacheResetReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextPrefixSegment {
    lane: ContextPrefixLane,
    digest: [u8; 32],
    bytes: u64,
}

/// Provider-projected cacheable prompt sequence.
///
/// Segments follow the provider's semantic prompt order rather than JSON object
/// field order. This makes an append-only message suffix preserve the previous
/// prefix even though the enclosing wire array gains a comma and closing bracket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPrefixSnapshot {
    profile: ContextPrefixProfile,
    segments: Vec<ContextPrefixSegment>,
    digest: String,
    bytes: u64,
    tokens: u64,
    cache_enabled: bool,
    compaction_digest: Option<[u8; 32]>,
}

impl ContextPrefixSnapshot {
    pub fn provider_neutral(request: &LlmRequest) -> Result<Self, crate::error::RuntimeError> {
        let mut builder =
            ContextPrefixBuilder::for_request(ContextPrefixProfile::ProviderNeutral, request);
        for tool in &request.tools {
            builder.push(ContextPrefixLane::Tools, tool)?;
        }
        if let Some(system) = &request.system {
            builder.push(ContextPrefixLane::Stable, system)?;
        }
        if let Some(schema) = &request.schema {
            builder.push(ContextPrefixLane::Stable, schema)?;
        }
        for message in &request.messages {
            builder.push(ContextPrefixLane::Messages, message)?;
        }
        Ok(builder.finish())
    }

    pub(crate) fn builder(
        profile: ContextPrefixProfile,
        request: &LlmRequest,
    ) -> ContextPrefixBuilder {
        ContextPrefixBuilder::for_request(profile, request)
    }

    pub fn initial_observation(&self) -> ContextCacheObservation {
        self.observation(0, 0, Some(self.default_reset_reason()))
    }

    fn default_reset_reason(&self) -> ContextCacheResetReason {
        if self.cache_enabled {
            ContextCacheResetReason::ColdStart
        } else {
            ContextCacheResetReason::CacheDisabled
        }
    }

    pub(crate) fn compare(
        &self,
        previous_provider: &str,
        current_provider: &str,
        previous_model: &str,
        current_model: &str,
        previous: &Self,
    ) -> ContextCacheObservation {
        let mut common_bytes = self.common_prefix(previous);
        let mut common_tokens = estimate_prefix_tokens(common_bytes);
        let reset_reason = if !self.cache_enabled {
            Some(ContextCacheResetReason::CacheDisabled)
        } else if previous_provider != current_provider {
            common_bytes = 0;
            common_tokens = 0;
            Some(ContextCacheResetReason::ProviderChanged)
        } else if previous_model != current_model {
            common_bytes = 0;
            common_tokens = 0;
            Some(ContextCacheResetReason::ModelChanged)
        } else if previous.profile != self.profile {
            common_bytes = 0;
            common_tokens = 0;
            Some(ContextCacheResetReason::ProjectionChanged)
        } else if !previous.cache_enabled {
            Some(ContextCacheResetReason::CacheEnabled)
        } else if self.lane_digest(ContextPrefixLane::Stable)
            != previous.lane_digest(ContextPrefixLane::Stable)
        {
            Some(ContextCacheResetReason::StableChanged)
        } else if self.lane_digest(ContextPrefixLane::Tools)
            != previous.lane_digest(ContextPrefixLane::Tools)
        {
            Some(ContextCacheResetReason::ToolsChanged)
        } else if !previous.is_segment_prefix_of(self) {
            Some(
                if self.compaction_digest.is_some()
                    && self.compaction_digest != previous.compaction_digest
                {
                    ContextCacheResetReason::Compaction
                } else {
                    ContextCacheResetReason::MessagePrefixChanged
                },
            )
        } else {
            None
        };
        self.observation(common_bytes, common_tokens, reset_reason)
    }

    fn common_prefix(&self, previous: &Self) -> u64 {
        self.segments
            .iter()
            .zip(&previous.segments)
            .take_while(|(current, old)| current == old)
            .fold(0u64, |bytes, (segment, _)| {
                bytes.saturating_add(segment.bytes)
            })
    }

    fn is_segment_prefix_of(&self, current: &Self) -> bool {
        self.segments.len() <= current.segments.len()
            && self
                .segments
                .iter()
                .zip(&current.segments)
                .all(|(old, new)| old == new)
    }

    fn lane_digest(&self, lane: ContextPrefixLane) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new();
        for segment in self.segments.iter().filter(|segment| segment.lane == lane) {
            hasher.update(&segment.digest);
            hasher.update(&segment.bytes.to_le_bytes());
        }
        hasher.finalize()
    }

    fn observation(
        &self,
        common_prefix_bytes: u64,
        common_prefix_tokens: u64,
        reset_reason: Option<ContextCacheResetReason>,
    ) -> ContextCacheObservation {
        ContextCacheObservation {
            profile: self.profile,
            wire_prefix_digest: self.digest.clone(),
            wire_prefix_bytes: self.bytes,
            wire_prefix_tokens: self.tokens,
            common_prefix_bytes,
            common_prefix_tokens,
            reset_reason,
        }
    }
}

pub(crate) struct ContextPrefixBuilder {
    profile: ContextPrefixProfile,
    segments: Vec<ContextPrefixSegment>,
    cache_enabled: bool,
    compaction_digest: Option<[u8; 32]>,
}

impl ContextPrefixBuilder {
    fn for_request(profile: ContextPrefixProfile, request: &LlmRequest) -> Self {
        let mut compaction_hasher = blake3::Hasher::new();
        let mut has_compaction = false;
        for part in request.messages.iter().flat_map(|message| &message.parts) {
            if let crate::message::MessagePart::CompactSummary {
                summary,
                seq_start,
                seq_end,
                count,
            } = part
            {
                has_compaction = true;
                compaction_hasher.update(&(summary.len() as u64).to_le_bytes());
                compaction_hasher.update(summary.as_bytes());
                compaction_hasher.update(&seq_start.to_le_bytes());
                compaction_hasher.update(&seq_end.to_le_bytes());
                compaction_hasher.update(&(*count as u64).to_le_bytes());
            }
        }
        Self {
            profile,
            segments: Vec::new(),
            cache_enabled: request.cache_prompt,
            compaction_digest: has_compaction.then(|| *compaction_hasher.finalize().as_bytes()),
        }
    }

    pub(crate) fn push<T: Serialize + ?Sized>(
        &mut self,
        lane: ContextPrefixLane,
        value: &T,
    ) -> Result<(), crate::error::RuntimeError> {
        let bytes = serde_json::to_vec(value).map_err(|error| {
            crate::error::RuntimeError::ToolFailed(format!(
                "serialize context prefix segment: {error}"
            ))
        })?;
        self.segments.push(ContextPrefixSegment {
            lane,
            digest: *blake3::hash(&bytes).as_bytes(),
            bytes: bytes.len() as u64,
        });
        Ok(())
    }

    pub(crate) fn finish(self) -> ContextPrefixSnapshot {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[self.profile as u8]);
        let mut bytes = 0u64;
        for segment in &self.segments {
            hasher.update(&[segment.lane as u8]);
            hasher.update(&segment.bytes.to_le_bytes());
            hasher.update(&segment.digest);
            bytes = bytes.saturating_add(segment.bytes);
        }
        ContextPrefixSnapshot {
            profile: self.profile,
            segments: self.segments,
            digest: format!("blake3:{}", hasher.finalize().to_hex()),
            bytes,
            tokens: estimate_prefix_tokens(bytes),
            cache_enabled: self.cache_enabled,
            compaction_digest: self.compaction_digest,
        }
    }
}

fn estimate_prefix_tokens(bytes: u64) -> u64 {
    ((bytes as f64) / 3.5).ceil() as u64
}

impl ModelContextPlan {
    pub fn new(request: LlmRequest) -> Self {
        Self::for_call(
            request,
            ContextCallPurpose::General,
            ContextCallIdentity::detached(),
        )
    }

    pub fn for_call(
        request: LlmRequest,
        call_purpose: ContextCallPurpose,
        call_identity: ContextCallIdentity,
    ) -> Self {
        let token_lanes = ContextTokenLanes::for_request(&request);
        Self {
            id: ContextPlanId::now(),
            request,
            token_lanes,
            call_purpose,
            call_identity,
        }
    }

    pub fn id(&self) -> &ContextPlanId {
        &self.id
    }

    pub fn request(&self) -> &LlmRequest {
        &self.request
    }

    pub fn token_lanes(&self) -> &ContextTokenLanes {
        &self.token_lanes
    }

    pub fn estimated_input_tokens(&self) -> u64 {
        self.token_lanes.total()
    }

    pub fn call_purpose(&self) -> ContextCallPurpose {
        self.call_purpose
    }

    pub fn call_identity(&self) -> &ContextCallIdentity {
        &self.call_identity
    }

    pub fn into_request(self) -> LlmRequest {
        self.request
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContextCallPurpose {
    #[default]
    General,
    Classification,
    Extraction,
    BranchGeneration,
    Compaction,
    InterjectionClassification,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContextCallScope {
    Root,
    Child,
    #[default]
    Detached,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ContextCallIdentity {
    pub scope: ContextCallScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_run_id: Option<crate::event::FlowRunId>,
}

impl ContextCallIdentity {
    pub fn detached() -> Self {
        Self::default()
    }

    pub(crate) fn from_tool_context(ctx: &crate::tool::ToolCtx) -> Self {
        let session_id = ctx.session_id.clone().or_else(|| {
            ctx.session_runtime
                .as_ref()
                .map(|session| session.id().to_string())
        });
        let scope = match ctx.history_segment {
            crate::tool::HistorySegment::Spawned => ContextCallScope::Child,
            crate::tool::HistorySegment::Root if session_id.is_some() => ContextCallScope::Root,
            crate::tool::HistorySegment::Root => ContextCallScope::Detached,
        };
        let flow_run_id = match scope {
            ContextCallScope::Root => None,
            ContextCallScope::Child | ContextCallScope::Detached => ctx.flow_run_id.clone(),
        };
        Self {
            scope,
            session_id,
            flow_run_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ContextUsageKey {
    pub provider: String,
    pub model: String,
    pub call_purpose: ContextCallPurpose,
    pub call_identity: ContextCallIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextUsageRecord {
    pub plan_id: ContextPlanId,
    pub usage: crate::provider::TokenUsage,
}

impl ContextUsageRecord {
    pub fn window_input_tokens(&self) -> u64 {
        self.usage.input.saturating_add(self.usage.cached_input)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextTokenLanes {
    pub stable: u64,
    pub tools: u64,
    pub messages: u64,
    pub records: u64,
}

impl ContextTokenLanes {
    pub fn for_request(request: &LlmRequest) -> Self {
        Self {
            stable: estimate_stable_tokens(&request.system),
            tools: estimate_tool_tokens(&request.tools),
            messages: crate::compaction::estimate_tokens_for_messages(&request.messages),
            records: 0,
        }
    }

    pub fn fixed_input_tokens(&self) -> u64 {
        self.stable.saturating_add(self.tools)
    }

    pub fn total(&self) -> u64 {
        self.fixed_input_tokens()
            .saturating_add(self.messages)
            .saturating_add(self.records)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenUsageSource {
    Provider,
    Estimated,
    Mixed,
}

pub fn estimate_fixed_input_tokens(
    system: &Option<String>,
    tools: &[crate::tool::ToolSpec],
) -> u64 {
    estimate_stable_tokens(system).saturating_add(estimate_tool_tokens(tools))
}

pub fn reconcile_token_usage(
    provider: &crate::provider::TokenUsage,
    estimated_input: u64,
    estimated_output: u64,
) -> (crate::provider::TokenUsage, TokenUsageSource) {
    let mut usage = provider.clone();
    let provider_reported = provider.input > 0
        || provider.cached_input > 0
        || provider.output > 0
        || provider.cache_write > 0
        || provider.reasoning_tokens > 0;
    let mut estimated = false;

    if usage.input.saturating_add(usage.cached_input) == 0 && estimated_input > 0 {
        usage.input = estimated_input;
        estimated = true;
    }
    if usage.output == 0 && estimated_output > 0 {
        usage.output = estimated_output;
        estimated = true;
    }

    let source = match (provider_reported, estimated) {
        (true, true) => TokenUsageSource::Mixed,
        (true, false) => TokenUsageSource::Provider,
        (false, _) => TokenUsageSource::Estimated,
    };
    (usage, source)
}

fn estimate_stable_tokens(system: &Option<String>) -> u64 {
    system
        .as_deref()
        .map(crate::provider::estimate_tokens)
        .unwrap_or(0)
}

fn estimate_tool_tokens(tools: &[crate::tool::ToolSpec]) -> u64 {
    serde_json::to_string(tools)
        .map(|json| crate::provider::estimate_tokens(&json))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LlmRequest {
        LlmRequest {
            model: "test-model".into(),
            messages: Vec::new(),
            system: Some("stable".into()),
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: true,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 120,
        }
    }

    #[test]
    fn plan_identity_is_unique_without_changing_request() {
        let first = ModelContextPlan::new(request());
        let second = ModelContextPlan::new(request());

        assert_ne!(first.id(), second.id());
        assert_eq!(first.request().model, "test-model");
        assert_eq!(first.request().system.as_deref(), Some("stable"));
        assert!(first.request().cache_prompt);
        assert_eq!(first.call_purpose(), ContextCallPurpose::General);
        assert_eq!(first.call_identity().scope, ContextCallScope::Detached);
    }

    #[test]
    fn token_lanes_cover_the_complete_request_without_structured_input() {
        let mut request = request();
        request.messages.push(crate::message::Message::user_text(
            crate::event::TurnId::now(),
            "hello",
        ));
        request.tools.push(crate::tool::ToolSpec {
            name: "fs.read".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::json!({"type": "object"}),
        });
        request.input = crate::Value::Str("not serialized".into());

        let plan = ModelContextPlan::new(request);
        assert!(plan.token_lanes().stable > 0);
        assert!(plan.token_lanes().tools > 0);
        assert!(plan.token_lanes().messages > 0);
        assert_eq!(plan.token_lanes().records, 0);
        assert_eq!(plan.estimated_input_tokens(), plan.token_lanes().total());
    }

    #[test]
    fn provider_cache_usage_is_not_inflated_by_plan_estimate() {
        let provider = crate::provider::TokenUsage {
            input: 40,
            cached_input: 60,
            output: 10,
            ..Default::default()
        };

        let (usage, source) = reconcile_token_usage(&provider, 100, 10);
        assert_eq!(usage.input, 40);
        assert_eq!(usage.cached_input, 60);
        assert_eq!(source, TokenUsageSource::Provider);
    }

    #[test]
    fn missing_provider_lanes_use_plan_estimates() {
        let provider = crate::provider::TokenUsage {
            reasoning_tokens: 5,
            ..Default::default()
        };

        let (usage, source) = reconcile_token_usage(&provider, 120, 20);
        assert_eq!(usage.input, 120);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.reasoning_tokens, 5);
        assert_eq!(source, TokenUsageSource::Mixed);
    }

    #[test]
    fn tool_context_identity_distinguishes_root_child_and_detached_calls() {
        let detached = ContextCallIdentity::from_tool_context(&crate::tool::ToolCtx::default());
        assert_eq!(detached.scope, ContextCallScope::Detached);

        let root = ContextCallIdentity::from_tool_context(&crate::tool::ToolCtx {
            session_id: Some("session-1".into()),
            flow_run_id: Some(crate::event::FlowRunId::now()),
            ..Default::default()
        });
        assert_eq!(root.scope, ContextCallScope::Root);
        assert_eq!(root.session_id.as_deref(), Some("session-1"));
        assert!(root.flow_run_id.is_none());

        let child = ContextCallIdentity::from_tool_context(&crate::tool::ToolCtx {
            session_id: Some("session-1".into()),
            flow_run_id: Some(crate::event::FlowRunId::now()),
            history_segment: crate::tool::HistorySegment::Spawned,
            ..Default::default()
        });
        assert_eq!(child.scope, ContextCallScope::Child);
        assert!(child.flow_run_id.is_some());
    }

    #[test]
    fn append_only_messages_preserve_the_previous_projected_prefix() {
        let mut first_request = request();
        first_request.cache_prompt = true;
        first_request
            .messages
            .push(crate::message::Message::user_text(
                crate::event::TurnId::now(),
                "first",
            ));
        let first = ContextPrefixSnapshot::provider_neutral(&first_request).unwrap();

        let mut second_request = first_request;
        second_request
            .messages
            .push(crate::message::Message::assistant_text(
                crate::event::TurnId::now(),
                "second",
            ));
        let second = ContextPrefixSnapshot::provider_neutral(&second_request).unwrap();
        let observation = second.compare("provider", "provider", "model", "model", &first);

        assert_eq!(observation.reset_reason, None);
        assert_eq!(observation.common_prefix_bytes, first.bytes);
        assert_eq!(observation.common_prefix_tokens, first.tokens);
    }

    #[test]
    fn cache_reset_reason_distinguishes_tools_compaction_and_model_changes() {
        let mut first_request = request();
        first_request.cache_prompt = true;
        first_request
            .messages
            .push(crate::message::Message::user_text(
                crate::event::TurnId::now(),
                "original",
            ));
        let first = ContextPrefixSnapshot::provider_neutral(&first_request).unwrap();

        let mut tools_request = first_request.clone();
        tools_request.tools.push(crate::tool::ToolSpec {
            name: "fs.read".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::json!({"type": "object"}),
        });
        let tools = ContextPrefixSnapshot::provider_neutral(&tools_request).unwrap();
        assert_eq!(
            tools
                .compare("provider", "provider", "model", "model", &first)
                .reset_reason,
            Some(ContextCacheResetReason::ToolsChanged)
        );

        let mut compact_request = first_request;
        compact_request.messages = vec![crate::message::Message::system_compact_summary(
            crate::event::TurnId::now(),
            "summary",
            1,
            2,
            2,
        )];
        let compact = ContextPrefixSnapshot::provider_neutral(&compact_request).unwrap();
        assert_eq!(
            compact
                .compare("provider", "provider", "model", "model", &first)
                .reset_reason,
            Some(ContextCacheResetReason::Compaction)
        );

        let mut compact_with_history = compact_request;
        compact_with_history
            .messages
            .push(crate::message::Message::user_text(
                crate::event::TurnId::now(),
                "old suffix",
            ));
        let previous_compact =
            ContextPrefixSnapshot::provider_neutral(&compact_with_history).unwrap();
        compact_with_history.messages[1] =
            crate::message::Message::user_text(crate::event::TurnId::now(), "rewritten suffix");
        let rewritten = ContextPrefixSnapshot::provider_neutral(&compact_with_history).unwrap();
        assert_eq!(
            rewritten
                .compare("provider", "provider", "model", "model", &previous_compact,)
                .reset_reason,
            Some(ContextCacheResetReason::MessagePrefixChanged)
        );
        assert_eq!(
            first
                .compare("provider", "provider", "old", "new", &first)
                .reset_reason,
            Some(ContextCacheResetReason::ModelChanged)
        );

        let mut disabled_request = request();
        disabled_request.cache_prompt = false;
        let disabled = ContextPrefixSnapshot::provider_neutral(&disabled_request).unwrap();
        disabled_request.cache_prompt = true;
        let enabled = ContextPrefixSnapshot::provider_neutral(&disabled_request).unwrap();
        assert_eq!(
            enabled
                .compare("provider", "provider", "model", "model", &disabled)
                .reset_reason,
            Some(ContextCacheResetReason::CacheEnabled)
        );
    }
}
