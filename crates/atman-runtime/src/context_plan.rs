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
}

impl ModelContextPlan {
    pub fn new(request: LlmRequest) -> Self {
        let token_lanes = ContextTokenLanes::for_request(&request);
        Self {
            id: ContextPlanId::now(),
            request,
            token_lanes,
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

    pub fn into_request(self) -> LlmRequest {
        self.request
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
}
