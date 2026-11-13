//! Entry-authored goal continuation uses the same inbox and consumption receipts as other input.

use std::io;

use atman_rt::{FlowInstance, TurnLease, TurnOutcome};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{FlowFact, FlowStore, completion::Completion, goal::GoalVersion, inbox::*, invalid};
use crate::{
    event::AtmanUuid,
    flow_store::position::FlowPosition,
    memory::goal::{GoalRecord, GoalStatus},
};

/// One request identity per source turn, acknowledged objective version and continuation payload.
pub fn goal_request(
    source: TurnLease<AtmanUuid>,
    goal: GoalVersion,
    text: String,
) -> io::Result<DeliveryRequest> {
    let key = serde_json::to_vec(&("goal-continuation", &source, &goal, &text)).map_err(invalid)?;
    request_with_id(
        source,
        goal,
        text,
        Uuid::from_bytes(Sha256::digest(key)[..16].try_into().unwrap()),
    )
}

fn legacy_goal_request(
    source: TurnLease<AtmanUuid>,
    goal: GoalVersion,
    text: String,
) -> io::Result<DeliveryRequest> {
    let key = serde_json::to_vec(&("goal-continuation", &source, &goal)).map_err(invalid)?;
    request_with_id(
        source,
        goal,
        text,
        Uuid::from_bytes(Sha256::digest(key)[..16].try_into().unwrap()),
    )
}

fn request_with_id(
    source: TurnLease<AtmanUuid>,
    goal: GoalVersion,
    text: String,
    request_id: Uuid,
) -> io::Result<DeliveryRequest> {
    Ok(DeliveryRequest {
        expected_head: None,
        request_id,
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
    if matches!(request.payload, InboxPayload::GoalResume { .. }) {
        return Err(invalid(
            "goal resume input must be committed with its goal update",
        ));
    }
    let InboxPayload::GoalContinuation { source, goal, text } = &request.payload else {
        return Ok(());
    };
    let mut canonical = goal_request(source.clone(), goal.clone(), text.clone())?;
    canonical.expected_head = request.expected_head.clone();
    if text.trim().is_empty()
        || !current.is_some_and(|record| {
            record.status == GoalStatus::Active && GoalVersion::from(record) == *goal
        })
        || !state.can_receive_at_boundary(source).unwrap_or(false)
        || &canonical != request
    {
        return Err(invalid(
            "continuation requires the active turn and current active goal",
        ));
    }
    Ok(())
}

/// Replay validates the immutable envelope contract while allowing historical lifecycle state
/// to be reconstructed by the following journal facts. New admissions remain strict.
pub(super) fn validate_replay_request(request: &DeliveryRequest) -> io::Result<()> {
    let InboxPayload::GoalContinuation { source, goal, text } = &request.payload else {
        return Err(invalid(
            "only goal continuations can use replay compatibility",
        ));
    };
    if text.trim().is_empty() {
        return Err(invalid("goal continuation text is empty"));
    }
    let canonical = goal_request(source.clone(), goal.clone(), text.clone())?;
    let legacy = legacy_goal_request(source.clone(), goal.clone(), text.clone())?;
    if &canonical != request && &legacy != request {
        return Err(invalid("goal continuation envelope is not canonical"));
    }
    Ok(())
}

impl Inbox {
    pub(super) fn goal_changed(&mut self, current: Option<&GoalRecord>, receipt: &FlowPosition) {
        self.cancel_matching(receipt, |envelope| {
            matches!(&envelope.request.payload, InputKind::GoalContinuation { goal, .. } | InputKind::GoalResume { goal, .. }
                if !current.is_some_and(|record| record.status == GoalStatus::Active && GoalVersion::from(record) == *goal))
        });
    }

    pub(super) fn turn_completed(&mut self, completion: &Completion, receipt: &FlowPosition) {
        self.cancel_matching(receipt, |envelope| {
            matches!(&envelope.request.payload, InputKind::GoalContinuation { source, .. }
                if source != &completion.lease || completion.outcome != TurnOutcome::Completed)
        });
    }
}

impl FlowStore {
    pub fn continuation_repair_candidates(&self) -> Vec<InboxRepairCancellation> {
        self.inbox
            .pending_entries()
            .filter_map(|entry| {
                let InputKind::GoalContinuation { source, goal } = &entry.envelope.request.payload
                else {
                    return None;
                };
                let source_failed = self.completions.get(source).is_some_and(|completion| {
                    completion.completion.outcome != TurnOutcome::Completed
                });
                let goal_invalid = !self.goal().is_some_and(|current| {
                    current.status == GoalStatus::Active && GoalVersion::from(current) == *goal
                });
                (source_failed || goal_invalid).then(|| InboxRepairCancellation {
                    message: entry.envelope.id,
                    expected: entry.revision.clone(),
                    source: source.clone(),
                    cause: InboxRepairCause::InvalidHistoricalContinuation,
                })
            })
            .collect()
    }

    pub fn repair_continuations(&mut self) -> io::Result<Vec<InboxRepairCancellation>> {
        let repairs = self.continuation_repair_candidates();
        if repairs.is_empty() {
            return Ok(repairs);
        }
        let facts: Vec<_> = repairs
            .iter()
            .cloned()
            .map(|repair| FlowFact::InboxRepairCancelled(Box::new(repair)))
            .collect();
        FlowFact::validate_batch(&facts)?;
        let receipt = self.journal.commit(&facts)?;
        for repair in &repairs {
            self.inbox.repair_cancel(repair, &receipt)?;
        }
        self.cursor = receipt;
        self.refresh_projection();
        Ok(repairs)
    }

    /// Authenticate the caller before this method. Matching retries retain the accepted disposition.
    /// A goal changed or paused after preparation declines the proposal without failing the entry.
    pub fn schedule_goal(
        &mut self,
        source: TurnLease<AtmanUuid>,
        goal: GoalVersion,
        text: String,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<Option<Acceptance>> {
        let mut request = goal_request(source.clone(), goal.clone(), text.clone())?;
        if let Some(head) = self
            .inbox
            .accepted_request_head(&request.sender, request.request_id)
        {
            request.expected_head = head.clone();
        }
        if let Some(receipt) = self.inbox.previous(&request)? {
            return Ok(Some(receipt));
        }
        let mut legacy = legacy_goal_request(source.clone(), goal.clone(), text.clone())?;
        if let Some(head) = self
            .inbox
            .accepted_request_head(&legacy.sender, legacy.request_id)
        {
            legacy.expected_head = head.clone();
        }
        match self.inbox.previous(&legacy) {
            Ok(Some(receipt)) => return Ok(Some(receipt)),
            Ok(None) => {}
            Err(error)
                if error
                    .to_string()
                    .contains("delivery request ID was reused with different data") => {}
            Err(error) => return Err(error),
        }
        if self.inbox.pending_goal_continuation(&source, &goal) {
            return Err(invalid(
                "a goal continuation is already pending for this source turn",
            ));
        }
        request.expected_head = Some(self.history_head());
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
        // The exact source lease was validated above; history cannot switch while it is active.
        self.accept_input(request, |_| Ok(())).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::{goal::GoalVersion, source_capture::tests::fixture};

    #[test]
    fn replay_accepts_current_and_legacy_request_ids_only_for_exact_payloads() {
        let (_directory, _flow, lease) = fixture();
        let goal = GoalVersion {
            id: "goal".into(),
            revision: 1,
        };
        let current = goal_request(lease.clone(), goal.clone(), "continue".into()).unwrap();
        let legacy = legacy_goal_request(lease.clone(), goal.clone(), "continue".into()).unwrap();
        assert!(validate_replay_request(&current).is_ok());
        assert!(validate_replay_request(&legacy).is_ok());

        let mut changed = legacy;
        changed.payload = InboxPayload::GoalContinuation {
            source: lease,
            goal,
            text: "changed".into(),
        };
        // The legacy identity intentionally omitted text, so replay cannot distinguish a
        // changed payload from its historical record. New admissions use the text-inclusive ID
        // and reject this case before it reaches the journal.
        assert!(validate_replay_request(&changed).is_ok());
        let mut current_changed = current;
        current_changed.payload = changed.payload.clone();
        assert!(validate_replay_request(&current_changed).is_err());
    }

    #[test]
    fn repair_cancellation_is_append_only_and_replays_as_cancelled() {
        use crate::memory::goal::GoalEdit;

        let (directory, mut flow, lease) = fixture();
        flow.edit_goal(
            crate::flow_store::goal::GoalRequest {
                id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                expected: None,
                expected_head: None,
                edit: GoalEdit::Create {
                    text: "objective".into(),
                },
                start_turn: false,
            },
            |_| Ok(()),
        )
        .unwrap();
        let goal = GoalVersion::from(flow.goal().unwrap());
        let accepted = flow
            .schedule_goal(lease.clone(), goal, "continue".into(), |_| Ok(()))
            .unwrap()
            .unwrap();
        let address = flow.state().address().clone();
        let repair = InboxRepairCancellation {
            message: accepted.message_id,
            expected: accepted.receipt.clone(),
            source: lease,
            cause: InboxRepairCause::InvalidHistoricalContinuation,
        };
        flow.journal
            .commit(&[FlowFact::InboxRepairCancelled(Box::new(repair))])
            .unwrap();
        drop(flow);

        let root = crate::flow_store::RuntimeStore::open(directory.path()).unwrap();
        let session = root.open_session(address.session_id.0).unwrap();
        let mut reopened = session.open_flow(&address.instance_id).unwrap();
        assert!(matches!(
            reopened
                .inbox_entry(accepted.message_id)
                .unwrap()
                .unwrap()
                .acceptance
                .disposition,
            InboxDisposition::Cancelled { .. }
        ));
        drop(reopened);
        assert!(
            session
                .continuation_repair_candidates(&address.instance_id)
                .unwrap()
                .is_empty()
        );
        assert!(
            session
                .repair_continuations(&address.instance_id)
                .unwrap()
                .is_empty()
        );
    }
}
