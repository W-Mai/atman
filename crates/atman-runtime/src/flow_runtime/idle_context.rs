//! Explicit context additions at an idle owner boundary without a synthetic invocation.

use std::{io, sync::Arc};

use atman_rt::{InstanceActivity, InstanceState};
use uuid::Uuid;

use super::FlowWorker;
use crate::{
    context_plan::{ContextRecordSpec, compile_context_records},
    event::TurnId,
    flow_store::{
        FlowRef,
        context::{
            CommittedContextCheckpoint, ContextCheckpoint, ContextCheckpointPurpose, context_digest,
        },
        invalid,
        position::FlowPosition,
    },
    message::{Message, MessageOrigin, MessageRole},
    tools::tool_output::OutputStore,
};

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct IdleContextUpdate {
    pub id: Uuid,
    pub expected: FlowPosition,
    pub turn: TurnId,
    pub records: Vec<ContextRecordSpec>,
}

impl FlowWorker {
    /// Append internal records only while idle. Retry the same request and captured outputs after an uncertain acknowledgement.
    pub async fn append_idle_context(
        &self,
        update: IdleContextUpdate,
        outputs: Arc<OutputStore>,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<CommittedContextCheckpoint> {
        let worker = self.clone();
        tokio::spawn(async move {
            let _launch = worker.launch_gate.clone().lock_owned().await;
            let encoded = serde_json::to_vec(&(&update.expected, &update.turn, &update.records))
                .map_err(invalid)?;
            let digest = blake3::hash(&encoded).to_hex().to_string();
            let context = worker.context();
            worker.request(move |store| {
                authorize(store.state().address())?;
                if update.id.is_nil() || update.records.is_empty() {
                    return Err(invalid("context update requires an identity and records"));
                }
                if let Some(saved) = store.context_checkpoint(update.id)? {
                    return match &saved.checkpoint.purpose {
                        ContextCheckpointPurpose::Append { request_digest } if request_digest == &digest => Ok(saved),
                        _ => Err(invalid("context update identity conflicts with recorded data")),
                    };
                }
                if store.cursor() != &update.expected
                    || !matches!(store.state().state(), InstanceState::Open(InstanceActivity::Idle))
                    || store.pending_inputs().next().is_some()
                {
                    return Err(invalid("context update requires the inspected idle conversation without pending input"));
                }
                let mut messages = store.context_messages()?;
                if messages.iter().rev().find(|message| {
                    message.role == MessageRole::User && message.origin == MessageOrigin::User
                }).is_none_or(|message| message.turn_id != update.turn) {
                    return Err(invalid("context update must belong to the existing latest user turn"));
                }
                let expected = context_digest(&messages)?;
                let start = messages.len();
                let before_tokens = crate::compaction::estimate_tokens_for_messages(&messages);
                let records = compile_context_records(&messages, update.records);
                messages.extend(records.into_iter().map(|record| Message::context_record(update.turn.clone(), record)));
                let after_tokens = crate::compaction::estimate_tokens_for_messages(&messages);
                // Assets become readable before their references enter the committed model window.
                store.output_store().import_snapshot(&outputs)?;
                let checkpoint = store.checkpoint_context(ContextCheckpoint {
                    expected_head: Some(store.history_head()),
                    id: update.id,
                    purpose: ContextCheckpointPurpose::Append { request_digest: digest },
                    expected,
                    messages,
                    summary: String::new(),
                    range: start..start,
                    count: 0,
                    before_tokens,
                    after_tokens,
                })?;
                context.project_checkpoint(&checkpoint.checkpoint);
                Ok(checkpoint)
            }).await
        }).await.map_err(io::Error::other)?
    }
}
