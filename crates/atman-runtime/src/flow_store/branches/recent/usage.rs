//! Usage is prepared for an exact invocation while checking its original journal.

use std::collections::BTreeMap;

use atman_rt::TurnLease;

use super::*;
use crate::{
    activity::{ActivityAccumulator, ActivitySummary, EditMetrics},
    context_plan::TokenUsageSource,
    event::{Event, LlmCallStatus},
    flow_store::trace::ExecutionEvent,
    provider::TokenUsage,
};

const MAX_EVENTS: usize = 4096;
const MAX_ROUTES: usize = 64;
const MAX_ROUTE_LABEL_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorkUsageRoute {
    pub provider: String,
    pub model: String,
    pub purpose: Option<&'static str>,
    pub scope: Option<&'static str>,
    pub source: WorkUsageSource,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum WorkUsageSource {
    Provider,
    Mixed,
    Estimated,
    Unknown,
}

#[derive(Default)]
pub struct WorkUsageTotals {
    pub calls: u32,
    pub failures: u32,
    pub cache_read_calls: u32,
    pub tokens: TokenUsage,
}

pub struct WorkUsage {
    pub routes: BTreeMap<WorkUsageRoute, WorkUsageTotals>,
    pub edits: ActivitySummary,
    pub interrupted_calls: u32,
}

pub(super) struct UsageCapture {
    lease: TurnLease<AtmanUuid>,
    through: u64,
    events: Vec<(u64, Contribution)>,
    omitted: bool,
}

enum Contribution {
    Call(WorkUsageRoute, TokenUsage, bool),
    Edit(String, EditMetrics),
    Interrupted,
    RouteLimit(usize),
}

impl UsageCapture {
    pub(super) fn new(lease: TurnLease<AtmanUuid>, through: u64) -> Self {
        Self {
            lease,
            through,
            events: Vec::new(),
            omitted: false,
        }
    }

    pub(super) fn observe(&mut self, commit: u64, facts: &[FlowFact]) {
        if commit > self.through || self.omitted {
            return;
        }
        let events = facts.iter().flat_map(|fact| match fact {
            FlowFact::ExecutionEvents(events) => events.as_slice(),
            _ => &[],
        });
        for event in events.filter(|event| event.scope.lease() == Some(&self.lease)) {
            if let Some(contribution) = contribution(event) {
                if self.events.len() == MAX_EVENTS {
                    self.omitted = true;
                    return;
                }
                self.events.push((commit, contribution));
            }
        }
    }

    fn summarize(self, visible: impl Fn(u64) -> bool) -> Result<WorkUsage, ConversationCapacity> {
        if self.omitted {
            return Err(capacity(MAX_EVENTS + 1, MAX_EVENTS));
        }
        let mut routes = BTreeMap::<_, WorkUsageTotals>::new();
        let mut edits = ActivityAccumulator::default();
        let mut interrupted_calls = 0;
        for (commit, contribution) in self.events {
            if !visible(commit) {
                continue;
            }
            match contribution {
                Contribution::Call(route, tokens, failed) => {
                    let totals = routes.entry(route).or_default();
                    totals.calls += 1;
                    totals.failures += u32::from(failed);
                    // Historical provider observations normalize an absent cache lane to zero.
                    totals.cache_read_calls += u32::from(tokens.cached_input > 0);
                    totals.tokens.input = totals.tokens.input.saturating_add(tokens.input);
                    totals.tokens.cached_input = totals
                        .tokens
                        .cached_input
                        .saturating_add(tokens.cached_input);
                    totals.tokens.cache_write =
                        totals.tokens.cache_write.saturating_add(tokens.cache_write);
                    totals.tokens.output = totals.tokens.output.saturating_add(tokens.output);
                    totals.tokens.reasoning_tokens = totals
                        .tokens
                        .reasoning_tokens
                        .saturating_add(tokens.reasoning_tokens);
                    if routes.len() > MAX_ROUTES {
                        return Err(capacity(routes.len(), MAX_ROUTES));
                    }
                }
                Contribution::Edit(path, metrics) => edits.observe_edit(&path, metrics),
                Contribution::Interrupted => interrupted_calls += 1,
                Contribution::RouteLimit(bytes) => {
                    return Err(capacity(bytes, MAX_ROUTE_LABEL_BYTES));
                }
            }
        }
        Ok(WorkUsage {
            routes,
            edits: edits.summary(),
            interrupted_calls,
        })
    }
}

fn capacity(required_at_least: usize, allowed: usize) -> ConversationCapacity {
    ConversationCapacity {
        kind: ConversationCapacityKind::SelectionWork,
        required_at_least,
        allowed,
        restart_required: false,
    }
}

fn contribution(record: &ExecutionEvent) -> Option<Contribution> {
    match &record.event {
        Event::LlmCall {
            model,
            provider,
            usage_source,
            context_call_purpose,
            context_call_identity,
            usage,
            status,
            ..
        } => {
            let bytes = model.len().max(provider.len());
            if bytes > MAX_ROUTE_LABEL_BYTES {
                return Some(Contribution::RouteLimit(bytes));
            }
            let source = match usage_source {
                Some(TokenUsageSource::Provider) => WorkUsageSource::Provider,
                Some(TokenUsageSource::Estimated) => WorkUsageSource::Estimated,
                Some(TokenUsageSource::Mixed) => WorkUsageSource::Mixed,
                None => WorkUsageSource::Unknown,
            };
            Some(Contribution::Call(
                WorkUsageRoute {
                    provider: provider.clone(),
                    model: model.clone(),
                    purpose: context_call_purpose.map(|purpose| purpose.as_str()),
                    scope: context_call_identity
                        .as_ref()
                        .map(|identity| identity.scope.as_str()),
                    source,
                },
                usage.clone(),
                !matches!(status, LlmCallStatus::Ok),
            ))
        }
        Event::LlmPartialCall { .. } => Some(Contribution::Interrupted),
        Event::FileEditApplied { path, metrics, .. } => {
            Some(Contribution::Edit(path.clone(), *metrics))
        }
        _ => None,
    }
}

impl PreparedConversationSource {
    pub(crate) fn read_work_usage(
        &mut self,
        item: ConversationItemId,
    ) -> io::Result<Result<WorkUsage, ConversationCapacity>> {
        let capture = self.usage.take().ok_or_else(missing_record)?;
        if item.record.item != 0 {
            return Err(missing_record());
        }
        let facts = match self.read_visible_batch(item.record.commit)? {
            Ok(facts) => facts,
            Err(capacity) => return Ok(Err(capacity)),
        };
        let owner = facts
            .get(item.record.fact)
            .filter(|fact| {
                fact.shared_messages()
                    .get(item.message)
                    .is_some_and(|message| message.role != MessageRole::System)
            })
            .and_then(ConversationWorkMember::from_fact);
        if !owner.is_some_and(|owner| owner.lease == capture.lease) {
            return Err(missing_record());
        }
        Ok(capture.summarize(|commit| self.visible(commit)))
    }
}
