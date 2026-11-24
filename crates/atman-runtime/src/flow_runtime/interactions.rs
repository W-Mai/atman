use std::io;

use super::FlowWorker;
use crate::{
    flow_store::branches::HistoryHead,
    session::{CompactReviewDecision, CompactReviewReceipt, PendingCompactReview},
};

impl FlowWorker {
    /// Observe the current review from the history selection used to prepare its summary.
    pub fn inspect_compact_review<T>(
        &self,
        head: &HistoryHead,
        project: impl FnOnce(&PendingCompactReview) -> T,
    ) -> Option<T> {
        let context = self.context();
        if context.history_head().as_ref() != Some(head) {
            return None;
        }
        context.compact_reviews().inspect_at(head, project)
    }

    /// Deliver a decision to the current review under this Flow's authority fence.
    pub async fn decide_compact_review(
        &self,
        review_id: String,
        head: HistoryHead,
        decision: CompactReviewDecision,
    ) -> io::Result<bool> {
        Ok(self
            .decide_compact_review_with_receipt(review_id, head, decision)
            .await?
            .is_some())
    }

    /// Deliver the decision and observe the originating compaction's eventual checkpoint.
    pub async fn decide_compact_review_with_receipt(
        &self,
        review_id: String,
        head: HistoryHead,
        decision: CompactReviewDecision,
    ) -> io::Result<Option<CompactReviewReceipt>> {
        let reviews = self.context().compact_reviews();
        self.request(move |store| {
            Ok((store.history_head() == head)
                .then(|| reviews.decide_at(&review_id, &head, decision))
                .flatten())
        })
        .await
    }
}
