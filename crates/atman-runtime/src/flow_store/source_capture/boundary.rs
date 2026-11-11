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
        let (source, boundary) = BoundarySource {
            flow: &self.descriptor.address,
            branch: ConversationBranchId::initial(
                &self.descriptor.address,
                self.cursor().journal_id(),
            ),
            inbox: &self.inbox,
            completions: &self.completions,
        }
        .resolve(&request.observed, &request.cutoff, |receipt| {
            self.journal.previous_durable_receipt(receipt)
        })?;
        let plan = self.capture_reference_plan(&ReferenceRequest {
            flow: source.flow,
            branch: Some(source.branch),
            at: Some(source.cursor),
            budget: request.budget,
        })?;
        Ok((plan, boundary))
    }
}

/// Original admission/completion indices resolve a boundary without preparing reference content.
pub(in crate::flow_store) struct BoundarySource<'a> {
    pub flow: &'a FlowRef,
    pub branch: ConversationBranchId,
    pub inbox: &'a crate::flow_store::Inbox,
    pub completions: &'a crate::flow_store::completion::Completions,
}

impl BoundarySource<'_> {
    pub fn resolve(
        &self,
        observed: &SourceSelection,
        cutoff: &ConversationCutoff,
        mut previous: impl FnMut(&CommitReceipt) -> io::Result<Option<CommitReceipt>>,
    ) -> io::Result<(SourceSelection, ResolvedConversationBoundary)> {
        if &observed.flow != self.flow || observed.branch != self.branch {
            return Err(invalid("conversation boundary belongs to another source"));
        }
        // The complete observation is validated even when the selected record is earlier.
        previous(&observed.cursor)?;
        let (lease, selected_receipt) = match cutoff {
            ConversationCutoff::BeforeUserTurn { lease, receipt, .. }
            | ConversationCutoff::AfterCompletedTurn { lease, receipt } => (lease, receipt),
        };
        if lease.flow != observed.flow || selected_receipt.sequence > observed.cursor.sequence {
            return Err(invalid(
                "conversation boundary exceeds its source observation",
            ));
        }
        let (cursor, admission) = match cutoff {
            ConversationCutoff::BeforeUserTurn {
                lease,
                input,
                receipt,
            } => {
                let inputs = self.inbox.inputs();
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
                let before = previous(admission)?
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
        Ok((
            SourceSelection {
                flow: observed.flow.clone(),
                branch: observed.branch,
                cursor,
            },
            ResolvedConversationBoundary {
                selected: cutoff.clone(),
                admission,
            },
        ))
    }
}

#[cfg(test)]
mod tests;
