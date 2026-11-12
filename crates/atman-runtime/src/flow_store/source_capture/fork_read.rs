//! Complete read-only history and checkpoint-selected context at one resolved Fork boundary.

use super::*;
use crate::{
    flow_store::{
        branches::ConversationView,
        conversation::{ConversationPlan, ConversationReadKind},
        position::FlowBatch,
        view::FlowUpdate,
    },
    projection::flow::FlowTranscript,
};

/// Bounds for source decoding, separate from destination asset-copy capacity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ForkReadBudget {
    pub max_batches: usize,
    pub max_bytes: usize,
}

impl Default for ForkReadBudget {
    fn default() -> Self {
        Self {
            max_batches: 32_768,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Independent read handles, not an accepted operation or a retained asset snapshot. Keep the source Session retention lease until all referenced assets are copied.
pub struct ForkConversationRead {
    boundary: ResolvedForkBoundary,
    directory: PathBuf,
    journal: CapturedJournal,
    selection: Arc<ConversationView>,
    model: ConversationPlan,
}

/// Original display records and model context remain separate and carry source identities. This read result contains source-owned asset references and cannot seed a destination directly.
pub struct ForkConversation {
    pub boundary: ResolvedForkBoundary,
    pub transcript: FlowTranscript,
    pub model_context: Vec<Message>,
}

impl FlowStore {
    /// Capture metadata only on the owner; prepare bodies on a read worker under source retention.
    pub fn capture_fork_conversation(
        &self,
        request: &ForkBoundaryRequest,
        budget: ForkReadBudget,
        authorize: impl FnOnce(&ForkBoundaryRequest) -> io::Result<()>,
    ) -> io::Result<ForkConversationRead> {
        let boundary = self.resolve_fork_boundary(request, authorize)?;
        if budget.max_batches == 0 || budget.max_bytes == 0 {
            return Err(invalid("Fork conversation requires a nonzero read budget"));
        }
        let source = &boundary.source;
        let selection = self
            .conversation_view_through(&source.cursor.clone().into())?
            .ok_or_else(|| invalid("Fork requires native conversation ancestry"))?;
        let model = self.conversation_plan_at(
            ConversationReadKind::ModelContext,
            source.branch,
            source.cursor.sequence,
        )?;
        let journal = self.journal.capture_durable_prefix(
            &source.cursor,
            budget.max_batches,
            budget.max_bytes,
        )?;
        Ok(ForkConversationRead {
            boundary,
            directory: self.require_directory()?.to_owned(),
            journal,
            selection: Arc::new(selection),
            model,
        })
    }
}

impl ForkConversationRead {
    /// Decode the captured prefix without executing facts or consulting the source's current head.
    pub fn prepare(self) -> io::Result<ForkConversation> {
        let mut transcript = FlowTranscript::new(self.boundary.source.flow.clone());
        transcript.bind_selection(Some(self.selection))?;
        for index in 0..self.journal.len() {
            let batch = self.journal.read::<FlowFact>(index)?;
            transcript.apply(FlowBatch {
                receipt: batch.receipt.into(),
                facts: super::super::images::resolve(&self.directory, &batch.facts)?
                    .into_iter()
                    .map(FlowUpdate::from)
                    .collect(),
            })?;
        }
        let model_context = self.model.read(|receipt| {
            let index = usize::try_from(
                receipt
                    .sequence()
                    .checked_sub(1)
                    .ok_or_else(|| invalid("Fork model receipt has no position"))?,
            )
            .map_err(invalid)?;
            let batch = self.journal.read::<FlowFact>(index)?;
            if receipt.require_durable()? != &batch.receipt {
                return Err(invalid(
                    "Fork model receipt differs from the captured source",
                ));
            }
            super::super::images::resolve(&self.directory, &batch.facts)
        })?;
        if !crate::message::tool_pairs_complete(&model_context) {
            return Err(invalid(
                "Fork model context contains an incomplete tool transaction",
            ));
        }
        Ok(ForkConversation {
            boundary: self.boundary,
            transcript,
            model_context,
        })
    }
}

#[cfg(test)]
mod tests;
