//! Stable turn cutoffs for bounded reference capture, without executable fork semantics.

use atman_rt::{TurnLease, TurnOutcome};

use super::*;
use crate::{
    event::AtmanUuid,
    flow_store::inbox::{InboxDisposition, InputTarget},
};

/// Select original admitted input or successful completion, never a visual row or live answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationCutoff {
    /// Inserted input selects the beginning of its containing invocation.
    BeforeUserTurn {
        lease: TurnLease<AtmanUuid>,
        input: Uuid,
        receipt: CommitReceipt,
    },
    AfterCompletedTurn {
        lease: TurnLease<AtmanUuid>,
        receipt: CommitReceipt,
    },
}

/// The observation fences all selected records while allowing later source appends.
#[derive(Clone, Debug)]
pub struct BoundaryReferenceRequest {
    pub observed: SourceSelection,
    pub cutoff: ConversationCutoff,
    pub budget: ReferenceBudget,
}

/// Normalization preserves the selected input and identifies its complete containing admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedConversationBoundary {
    pub selected: ConversationCutoff,
    /// Present for BeforeUserTurn; includes every input admitted atomically with that turn.
    pub admission: Option<CommitReceipt>,
}

/// Reference omissions remain explicit; a turn cutoff does not certify complete fork history.
#[derive(Clone)]
pub struct BoundaryReferenceSnapshot {
    pub boundary: ResolvedConversationBoundary,
    pub reference: ReferenceSnapshot,
}

impl FlowStore {
    pub(crate) fn capture_reference_boundary_plan(
        &self,
        request: &BoundaryReferenceRequest,
    ) -> io::Result<(ReferencePlan, ResolvedConversationBoundary)> {
        request.budget.validate()?;
        let observed = &request.observed;
        let current = self.cursor().require_durable()?;
        if observed.flow != self.descriptor.address
            || observed.branch != ConversationBranchId::initial(&observed.flow, current.journal_id)
        {
            return Err(invalid("conversation boundary belongs to another source"));
        }
        // The complete observation is validated even when the selected record is earlier.
        self.journal.previous_durable_receipt(&observed.cursor)?;
        let (lease, selected_receipt) = match &request.cutoff {
            ConversationCutoff::BeforeUserTurn { lease, receipt, .. }
            | ConversationCutoff::AfterCompletedTurn { lease, receipt } => (lease, receipt),
        };
        if lease.flow != observed.flow || selected_receipt.sequence > observed.cursor.sequence {
            return Err(invalid(
                "conversation boundary exceeds its source observation",
            ));
        }
        let (cursor, admission) = match &request.cutoff {
            ConversationCutoff::BeforeUserTurn {
                lease,
                input,
                receipt,
            } => {
                let inputs = self.committed_inputs();
                let index = inputs
                    .binary_search_by_key(&receipt.sequence, |record| record.receipt.sequence())
                    .map_err(|_| invalid("selected user input has no committed admission"))?;
                let selected = &inputs[index];
                if &selected.lease != lease
                    || selected.receipt.require_durable()? != receipt
                    || !selected.message_ids.contains(input)
                    || !matches!(
                        selected.target,
                        InputTarget::InvocationInput | InputTarget::Context
                    )
                    || !self.inbox.summary(*input).is_some_and(|entry| {
                        entry.envelope.request.is_editable_user_input()
                            && matches!(&entry.acceptance.disposition,
                                InboxDisposition::InputCommitted { by } if by == &selected.receipt)
                    })
                {
                    return Err(invalid("selected input is not the committed User turn"));
                }
                // Turn epochs are monotonic; Context inputs cannot become separate turn boundaries.
                let start = inputs.partition_point(|record| record.lease.epoch < lease.epoch);
                let admitted = inputs
                    .get(start)
                    .filter(|record| {
                        &record.lease == lease && record.target == InputTarget::InvocationInput
                    })
                    .ok_or_else(|| invalid("selected input has no containing turn admission"))?;
                let admission = admitted.receipt.require_durable()?;
                let before = self
                    .journal
                    .previous_durable_receipt(admission)?
                    .ok_or_else(|| invalid("turn admission has no committed predecessor"))?;
                (before, Some(admission.clone()))
            }
            ConversationCutoff::AfterCompletedTurn { lease, receipt } => {
                let completion = self
                    .completions
                    .get(lease)
                    .filter(|record| {
                        &record.completion.lease == lease
                            && record.completion.outcome == TurnOutcome::Completed
                    })
                    .ok_or_else(|| invalid("selected turn has not completed successfully"))?;
                if completion.receipt.require_durable()? != receipt {
                    return Err(invalid(
                        "turn completion receipt does not match its history",
                    ));
                }
                (receipt.clone(), None)
            }
        };
        let plan = self.capture_reference_plan(&ReferenceRequest {
            flow: observed.flow.clone(),
            branch: Some(observed.branch),
            at: Some(cursor),
            budget: request.budget,
        })?;
        Ok((
            plan,
            ResolvedConversationBoundary {
                selected: request.cutoff.clone(),
                admission,
            },
        ))
    }
}

#[cfg(test)]
mod tests;
