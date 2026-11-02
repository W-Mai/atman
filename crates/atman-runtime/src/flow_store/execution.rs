//! Receipt-bound execution observations; live clocks never enter the journal.

use std::time::Instant;

use atman_rt::{ActiveTurn, FlowInstance, InstanceActivity, InstanceState, TurnLease};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{FlowStore, trace::ExecutionEvent};
use crate::{
    event::{AtmanUuid, Event},
    event_log::journal::CommitReceipt,
};

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionObservation {
    pub observed_at: DateTime<Utc>,
    pub receipt: CommitReceipt,
    pub loaded: bool,
    pub active: Option<ActiveExecutionObservation>,
    pub last_execution_event: Option<ExecutionEventRef>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActiveExecutionObservation {
    pub lease: TurnLease<AtmanUuid>,
    pub timing: TurnTiming,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnTiming {
    Local {
        admitted_at: DateTime<Utc>,
        elapsed_ms: u64,
    },
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionEventRef {
    pub lease: TurnLease<AtmanUuid>,
    pub sequence: u64,
    pub at: DateTime<Utc>,
    pub receipt: CommitReceipt,
    pub kind: ExecutionEventKind,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEventKind {
    Model,
    Tool,
    Flow,
    Approval,
    Context,
    Diagnostic,
}

impl ExecutionEventRef {
    pub(super) fn capture(event: &ExecutionEvent, receipt: &CommitReceipt) -> Option<Self> {
        let lease = event.scope.lease()?.clone();
        let kind = match &event.event {
            Event::LlmCall { .. } | Event::LlmPartialCall { .. } => ExecutionEventKind::Model,
            Event::ToolNode { .. }
            | Event::ToolResultMetrics { .. }
            | Event::FileEditApplied { .. } => ExecutionEventKind::Tool,
            Event::FlowStart { .. }
            | Event::FlowEnd { .. }
            | Event::FlowNodeStart { .. }
            | Event::FlowNodeEnd { .. }
            | Event::FlowGraph { .. } => ExecutionEventKind::Flow,
            Event::ToolPendingApproval { .. }
            | Event::ToolApproved { .. }
            | Event::ToolDenied { .. }
            | Event::PendingPrompt { .. }
            | Event::PromptResolved { .. }
            | Event::PromptExpired { .. } => ExecutionEventKind::Approval,
            Event::ContextTruncated { .. } | Event::ContentFilterHit { .. } => {
                ExecutionEventKind::Context
            }
            _ => ExecutionEventKind::Diagnostic,
        };
        Some(Self {
            lease,
            sequence: event.sequence,
            at: event.timestamp,
            receipt: receipt.clone(),
            kind,
        })
    }
}

pub(super) struct LiveTurnClock {
    lease: TurnLease<AtmanUuid>,
    admitted_at: DateTime<Utc>,
    started: Instant,
}

pub(super) fn active(state: &FlowInstance<AtmanUuid>) -> Option<&ActiveTurn<AtmanUuid>> {
    match state.state() {
        InstanceState::Open(InstanceActivity::Active(active))
        | InstanceState::Closing {
            active: Some(active),
        } => Some(active),
        _ => None,
    }
}

impl ExecutionObservation {
    pub(super) fn capture(
        state: &FlowInstance<AtmanUuid>,
        receipt: &CommitReceipt,
        latest: Option<&ExecutionEventRef>,
        clock: Option<&LiveTurnClock>,
        loaded: bool,
    ) -> Self {
        let active = active(state);
        let timing = clock
            .filter(|clock| active.is_some_and(|active| active.lease == clock.lease))
            .map_or(TurnTiming::Unknown, |clock| TurnTiming::Local {
                admitted_at: clock.admitted_at,
                elapsed_ms: clock
                    .started
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            });
        Self {
            observed_at: Utc::now(),
            receipt: receipt.clone(),
            loaded,
            active: active.map(|active| ActiveExecutionObservation {
                lease: active.lease.clone(),
                timing,
            }),
            last_execution_event: latest
                .filter(|event| active.is_none_or(|active| active.lease == event.lease))
                .cloned(),
        }
    }
}

impl FlowStore {
    /// Install only a successfully committed candidate. All live state mutations share this boundary.
    pub(super) fn install_state(&mut self, candidate: FlowInstance<AtmanUuid>) {
        if active(&self.state).map(|turn| &turn.lease) != active(&candidate).map(|turn| &turn.lease)
        {
            self.live_turn_clock = active(&candidate).map(|turn| LiveTurnClock {
                lease: turn.lease.clone(),
                admitted_at: Utc::now(),
                started: Instant::now(),
            });
        }
        self.state = candidate;
    }

    /// Reading never advances a revision, starts a clock, or publishes a heartbeat.
    pub fn execution_observation(&self) -> ExecutionObservation {
        ExecutionObservation::capture(
            &self.state,
            &self.cursor,
            self.trace.latest.as_ref(),
            self.live_turn_clock.as_ref(),
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_uses_monotonic_admission_time_instead_of_wall_clock() {
        let address = super::super::FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: super::super::FlowInstanceId::now(),
        };
        let mut state = FlowInstance::prepared(address);
        state.activate().unwrap();
        let lease = state
            .begin_turn(atman_rt::TurnId::now(), atman_rt::RunId::now())
            .unwrap();
        let clock = LiveTurnClock {
            lease,
            admitted_at: Utc::now() + chrono::Duration::days(1),
            started: Instant::now() - std::time::Duration::from_secs(20),
        };
        let receipt = CommitReceipt {
            journal_id: uuid::Uuid::now_v7(),
            sequence: 1,
            digest: "test".into(),
        };
        let sample = ExecutionObservation::capture(&state, &receipt, None, Some(&clock), true);
        assert!(
            matches!(sample.active.unwrap().timing, TurnTiming::Local { elapsed_ms, .. } if elapsed_ms >= 20_000)
        );
        assert!(matches!(
            ExecutionObservation::capture(&state, &receipt, None, None, false)
                .active
                .unwrap()
                .timing,
            TurnTiming::Unknown
        ));
    }
}
