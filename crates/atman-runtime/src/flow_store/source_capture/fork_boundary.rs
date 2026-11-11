//! Read-only selection of complete turn boundaries before independent Fork snapshot preparation.

use super::*;
use crate::flow_store::{branches::HistoryHead, inbox::InputTarget};
use atman_rt::TurnOutcome;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "boundary", rename_all = "snake_case")]
pub enum ForkCutoff {
    LatestSafe,
    At(Box<ConversationCutoff>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ForkBoundaryRequest {
    pub observed: SourceSelection,
    pub expected_head: HistoryHead,
    pub cutoff: ForkCutoff,
}

/// This metadata is not a retained snapshot or a destination execution grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedForkBoundary {
    pub observed: SourceSelection,
    pub head: HistoryHead,
    pub source: SourceSelection,
    /// None denotes the original baseline before any successfully completed turn.
    pub boundary: Option<ResolvedConversationBoundary>,
    pub excluded_turns: usize,
}

impl FlowStore {
    /// Resolve only acknowledged metadata; full history, tool closure and assets require separate capture.
    pub fn resolve_fork_boundary(
        &self,
        request: &ForkBoundaryRequest,
        authorize: impl FnOnce(&ForkBoundaryRequest) -> io::Result<()>,
    ) -> io::Result<ResolvedForkBoundary> {
        authorize(request)?;
        if request.observed.flow != self.descriptor.address
            || !matches!(self.descriptor.content, super::super::FlowContent::Entry(_))
        {
            return Err(invalid("Fork requires the selected native conversation"));
        }
        let head = self.history_head();
        head.validate_write(Some(&request.expected_head))?;
        if request.observed.branch != head.branch {
            return Err(invalid("Fork observation belongs to another branch"));
        }
        self.journal
            .previous_durable_receipt(&request.observed.cursor)?;
        let history = &self
            .branches
            .as_ref()
            .ok_or_else(|| invalid("Fork requires native conversation ancestry"))?
            .history;
        if !history.contains(head.branch, request.observed.cursor.sequence)? {
            return Err(invalid("Fork observation is outside the selected ancestry"));
        }
        let spans = history.ancestry_through(head.branch, request.observed.cursor.sequence)?;
        let visible = |sequence| {
            spans
                .get(spans.partition_point(|span| span.last_sequence < sequence))
                .is_some_and(|span| span.first_sequence <= sequence)
        };
        let selected = match &request.cutoff {
            ForkCutoff::At(cutoff) => Some(cutoff.as_ref().clone()),
            ForkCutoff::LatestSafe => reverse_ancestry(self.completions(), &spans, 0, |record| {
                record.receipt.sequence()
            })
            .find(|record| record.completion.outcome == TurnOutcome::Completed)
            .map(|record| {
                Ok::<_, io::Error>(ConversationCutoff::AfterCompletedTurn {
                    lease: record.completion.lease.clone(),
                    receipt: record.receipt.require_durable()?.clone(),
                })
            })
            .transpose()?,
        };
        let (source, boundary) = if let Some(selected) = selected {
            let (source, boundary) = BoundarySource {
                flow: &self.descriptor.address,
                branch: head.branch,
                inbox: &self.inbox,
                completions: &self.completions,
            }
            .resolve(&request.observed, &selected, history, |receipt| {
                self.journal.previous_durable_receipt(receipt)
            })?;
            (source, Some(boundary))
        } else {
            let initial = ConversationBranchId::initial(
                &request.observed.flow,
                request.observed.cursor.journal_id,
            );
            let baseline = self
                .activation
                .as_ref()
                .map(|(_, receipt)| receipt)
                .filter(|receipt| visible(receipt.sequence()))
                .map(|receipt| receipt.require_durable().cloned())
                .transpose()?
                .unwrap_or(history.branch(initial)?.created.clone());
            (
                SourceSelection {
                    cursor: baseline,
                    ..request.observed.clone()
                },
                None,
            )
        };
        let excluded_turns = reverse_ancestry(
            self.committed_inputs(),
            &spans,
            source.cursor.sequence,
            |record| record.receipt.sequence(),
        )
        .filter(|record| record.target == InputTarget::InvocationInput)
        .count();
        Ok(ResolvedForkBoundary {
            observed: request.observed.clone(),
            head,
            source,
            boundary,
            excluded_turns,
        })
    }
}

#[cfg(test)]
mod tests;
