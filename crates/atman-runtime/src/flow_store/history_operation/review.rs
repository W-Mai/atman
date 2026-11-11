//! Read-only operation review from the same owner indices used by admission.

use super::*;
use crate::flow_store::inbox::InputTarget;

/// Client intent retains its displayed head and boundary; the owner captures mutable review state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryReviewRequest {
    pub id: Uuid,
    pub sender: SenderIdentity,
    pub flow: FlowRef,
    pub expected_head: HistoryHead,
    pub target: HistoryOperationTarget,
}

/// Confirmation submits the captured request unchanged; review alone never holds admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryOperationReview {
    pub request: HistoryOperationRequest,
    pub selection: HistorySelection,
    /// Number of invocation admissions removed by Revert; absent for Restore.
    pub affected_turns: Option<usize>,
    /// Every original input admitted together at a selected User-turn boundary.
    pub admitted_inputs: Vec<Uuid>,
}

impl FlowStore {
    /// Authenticate before inspecting metadata. No message bodies are loaded or facts appended.
    pub fn review_history_operation(
        &self,
        intent: HistoryReviewRequest,
        authorize: impl FnOnce(&HistoryReviewRequest) -> io::Result<()>,
    ) -> io::Result<HistoryOperationReview> {
        authorize(&intent)?;
        let request = HistoryOperationRequest {
            id: intent.id,
            sender: intent.sender,
            flow: intent.flow,
            expected_head: intent.expected_head,
            execution_revision: self.state.revision(),
            active: match self.state.state() {
                InstanceState::Open(InstanceActivity::Active(active)) => Some(active.lease.clone()),
                _ => None,
            },
            goal: self.goal().map(GoalVersion::from),
            queued: self
                .pending_input_entries()
                .map(|entry| HistoryQueueItem {
                    message: entry.envelope.id,
                    revision: entry.revision.clone(),
                })
                .collect(),
            target: intent.target,
        };
        let selection = self.prepare_history_operation(&request)?;
        let mut admitted_inputs = Vec::new();
        let affected_turns = match &selection {
            HistorySelection::Create { parent, .. } => {
                let history = &self
                    .branches
                    .as_ref()
                    .ok_or_else(|| invalid("history review requires native ancestry"))?
                    .history;
                let inputs = self.committed_inputs();
                let mut count = 0;
                for span in history.ancestry_through(parent.branch, self.cursor.sequence())? {
                    if span.last_sequence <= parent.cursor.sequence {
                        continue;
                    }
                    let first = inputs.partition_point(|record| {
                        record.receipt.sequence() < span.first_sequence
                            || record.receipt.sequence() <= parent.cursor.sequence
                    });
                    let end = inputs
                        .partition_point(|record| record.receipt.sequence() <= span.last_sequence);
                    for record in &inputs[first..end] {
                        if record.target == InputTarget::InvocationInput {
                            count += 1;
                            if let HistoryOperationTarget::Revert { cutoff, .. } = &request.target
                                && let ConversationCutoff::BeforeUserTurn { lease, .. } = &**cutoff
                                && &record.lease == lease
                            {
                                admitted_inputs.clone_from(&record.message_ids);
                            }
                        }
                    }
                }
                Some(count)
            }
            HistorySelection::Restore { .. } => None,
        };
        Ok(HistoryOperationReview {
            request,
            selection,
            affected_turns,
            admitted_inputs,
        })
    }
}

#[cfg(test)]
mod tests;
