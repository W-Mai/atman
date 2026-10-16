//! Entry-authored goal continuation uses the same inbox and consumption receipts as other input.

use std::io;

use atman_rt::{FlowInstance, TurnLease, TurnOutcome};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{FlowStore, completion::Completion, goal::GoalVersion, inbox::*, invalid};
use crate::{
    event::AtmanUuid,
    event_log::journal::CommitReceipt,
    memory::goal::{GoalRecord, GoalStatus},
};

/// One request identity per source turn and acknowledged objective version.
pub fn goal_request(
    source: TurnLease<AtmanUuid>,
    goal: GoalVersion,
    text: String,
) -> io::Result<DeliveryRequest> {
    let key = serde_json::to_vec(&("goal-continuation", &source, &goal)).map_err(invalid)?;
    let digest = Sha256::digest(key);
    Ok(DeliveryRequest {
        request_id: Uuid::from_bytes(digest[..16].try_into().unwrap()),
        sender: SenderIdentity::Flow(source.flow.clone()),
        recipient: source.flow.clone(),
        delivery: DeliveryMode::NextTurn,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::GoalContinuation { source, goal, text },
    })
}

pub(super) fn validate_request(
    request: &DeliveryRequest,
    state: &FlowInstance<AtmanUuid>,
    current: Option<&GoalRecord>,
) -> io::Result<()> {
    let InboxPayload::GoalContinuation { source, goal, text } = &request.payload else {
        return Ok(());
    };
    if text.trim().is_empty()
        || !current.is_some_and(|record| {
            record.status == GoalStatus::Active && GoalVersion::from(record) == *goal
        })
        || !state.can_receive_at_boundary(source).unwrap_or(false)
        || &goal_request(source.clone(), goal.clone(), text.clone())? != request
    {
        return Err(invalid(
            "continuation requires the active turn and current active goal",
        ));
    }
    Ok(())
}

impl Inbox {
    pub(super) fn goal_changed(&mut self, current: Option<&GoalRecord>, receipt: &CommitReceipt) {
        self.cancel_matching(receipt, |envelope| {
            matches!(&envelope.request.payload, InboxPayload::GoalContinuation { goal, .. }
                if !current.is_some_and(|record| record.status == GoalStatus::Active && GoalVersion::from(record) == *goal))
        });
    }

    pub(super) fn turn_completed(&mut self, completion: &Completion, receipt: &CommitReceipt) {
        self.cancel_matching(receipt, |envelope| {
            matches!(&envelope.request.payload, InboxPayload::GoalContinuation { source, .. }
                if source != &completion.lease || completion.outcome != TurnOutcome::Completed)
        });
    }
}

impl FlowStore {
    /// Authenticate the caller before this method. Matching retries retain the accepted disposition.
    /// A goal changed or paused after preparation declines the proposal without failing the entry.
    pub fn schedule_goal(
        &mut self,
        source: TurnLease<AtmanUuid>,
        goal: GoalVersion,
        text: String,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<Option<Acceptance>> {
        let request = goal_request(source.clone(), goal.clone(), text.clone())?;
        if let Some(receipt) = self.inbox.previous(&request)? {
            return Ok(Some(receipt));
        }
        authorize_new(&request)?;
        if text.trim().is_empty() || !self.state.can_receive_at_boundary(&source).unwrap_or(false) {
            return Err(invalid(
                "continuation requires a live turn and nonempty text",
            ));
        }
        if !self.goal().is_some_and(|record| {
            record.status == GoalStatus::Active && GoalVersion::from(record) == goal
        }) {
            return Ok(None);
        }
        self.accept_input(request, |_| Ok(())).map(Some)
    }
}
