//! Activity derived from timestamped journal facts, shared by live and read-only projections.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{FlowFact, watch_view::WatchRegistration};
use crate::flow_store::position::FlowPosition;

/// Greatest recorded activity timestamp and its source commit. Untimed control/publication facts, unobserved completion recovery, observation cursors without a notification, legacy import time, and filesystem timestamps do not advance this value. It does not indicate whether execution is currently running.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecordedActivity {
    pub at: DateTime<Utc>,
    pub receipt: FlowPosition,
}

pub(super) fn observe(
    current: &mut Option<RecordedActivity>,
    receipt: &FlowPosition,
    facts: &[FlowFact],
) {
    for fact in facts {
        let at = match fact {
            FlowFact::Activated(record) => Some(record.accepted_at),
            FlowFact::EnvelopeAccepted(record) => Some(record.accepted_at),
            FlowFact::SendQueued(record) => Some(record.queued_at),
            FlowFact::TurnCompleted(record)
                if !matches!(
                    record.result,
                    super::completion::InvocationResult::Unobserved(_)
                ) =>
            {
                Some(record.finished_at)
            }
            FlowFact::ExecutionEvents(events) => events.iter().map(|event| event.timestamp).max(),
            FlowFact::GoalUpdated(record) => Some(record.at),
            FlowFact::Controlled(record) => Some(record.at),
            FlowFact::PlanningUpdated(record) => Some(record.at),
            FlowFact::NameUpdated(record) => Some(record.at),
            FlowFact::FlowSubscribed(record) => Some(record.registered_at),
            FlowFact::ProcessWatchRegistered(record) => Some(record.registered_at),
            FlowFact::TimerScheduled(record) => Some(record.scheduled_at),
            FlowFact::TaskScopeCreated(record) => Some(record.at),
            FlowFact::TimersFired(record) => Some(record.at),
            FlowFact::WatcherReplaced(record) => Some(match &record.replacement {
                WatchRegistration::Flow(record) => record.registered_at,
                WatchRegistration::Process(record) => record.registered_at,
            }),
            FlowFact::EventsObserved(record) if !record.envelopes.is_empty() => {
                Some(record.observed_at)
            }
            FlowFact::ProcessObserved(record) if !record.envelopes.is_empty() => {
                Some(record.report.observed_at)
            }
            FlowFact::FlowWatchesExpired(record) if !record.envelopes.is_empty() => Some(record.at),
            FlowFact::ProcessWatchesExpired(record) if !record.envelopes.is_empty() => {
                Some(record.at)
            }
            _ => None,
        };
        if let Some(at) = at
            && current.as_ref().is_none_or(|current| at >= current.at)
        {
            *current = Some(RecordedActivity {
                at,
                receipt: receipt.clone(),
            });
        }
    }
}
