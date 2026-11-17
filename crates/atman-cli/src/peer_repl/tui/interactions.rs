//! Retain original native interaction observations when their view receivers close.

use super::*;

#[cfg(test)]
mod tests;

pub(super) struct Observations {
    _forms: tokio::sync::watch::Receiver<Vec<atman_runtime::form::PendingForm>>,
    compact: HashMap<atman_runtime::flow_store::FlowRef, CompactObservation>,
}

struct CompactObservation {
    context: std::sync::Weak<atman_runtime::Session>,
    reviews: Arc<atman_runtime::session::CompactReviewRegistry>,
    changes: tokio::sync::watch::Receiver<Option<atman_runtime::session::PendingCompactReview>>,
    displayed: Option<Uuid>,
}

impl Observations {
    pub fn new(
        prompts: &prompts::Prompts,
        forms: &Arc<atman_runtime::session::FormRegistry>,
    ) -> Self {
        Self {
            _forms: prompts.observe_forms(forms),
            compact: HashMap::new(),
        }
    }

    pub fn observe(&mut self, worker: &FlowWorker) {
        let context = worker.context();
        let reviews = context.compact_reviews();
        if self
            .compact
            .get(worker.address())
            .is_some_and(|observation| Arc::ptr_eq(&observation.reviews, &reviews))
        {
            return;
        }
        self.compact.insert(
            worker.address().clone(),
            CompactObservation {
                context: Arc::downgrade(&context),
                changes: reviews.subscribe(),
                reviews,
                displayed: None,
            },
        );
    }

    pub fn update(&mut self) {
        self.compact.retain(|_, observation| {
            let Some(context) = observation.context.upgrade() else {
                return false;
            };
            let pending = observation.changes.borrow_and_update();
            let current = pending
                .as_ref()
                .and_then(|review| review.review_id.parse::<Uuid>().ok());
            if observation.displayed == current {
                return true;
            }
            if let Some(prompt_id) = observation.displayed.take() {
                context
                    .sink()
                    .emit(atman_runtime::event::Event::PromptExpired { prompt_id });
            }
            if let Some(review) = pending.as_ref()
                && let Some(prompt_id) = current
            {
                context
                    .sink()
                    .emit(atman_runtime::event::Event::PendingPrompt {
                        prompt_id,
                        kind: "compact_review".into(),
                        payload: serde_json::json!({
                            "review_id": review.review_id,
                            "summary": review.summary,
                            "slice_preview": review.slice_preview,
                            "slice_count": review.slice_count,
                            "range_start": review.range_start,
                            "range_end": review.range_end,
                            "tokens_before": review.tokens_before,
                            "emitted_at": review.emitted_at,
                        }),
                    });
                observation.displayed = Some(prompt_id);
            }
            true
        });
    }

    pub async fn changed(&mut self) {
        let changes = self
            .compact
            .values_mut()
            .map(|observation| Box::pin(observation.changes.changed()))
            .collect::<Vec<_>>();
        if changes.is_empty() {
            std::future::pending().await
        } else {
            let _ = futures::future::select_all(changes).await;
        }
    }
}
