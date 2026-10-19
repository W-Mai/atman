//! Profile-owned control intents and per-target delivery progress.

use std::{
    collections::{HashMap, HashSet},
    io,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Fact, ProfileStore};
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::{FlowRef, control::ControlRequest, inbox::SenderIdentity, invalid};

/// The host binds the actor and snapshots the targets before recording the batch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlBatchRequest {
    pub id: Uuid,
    pub sender: SenderIdentity,
    pub commands: Vec<ControlRequest>,
}

/// Acceptance and application cite target journal facts, not model-reported success.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ControlProgress {
    Accepted { receipt: CommitReceipt },
    Applied { receipt: CommitReceipt },
    Failed { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlProgressRecord {
    pub progress: ControlProgress,
    pub recorded: CommitReceipt,
}

/// An empty history means PendingDelivery. Terminal progress retains prior acceptance evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlBatchItem {
    pub request: ControlRequest,
    pub history: Vec<ControlProgressRecord>,
}

impl ControlBatchItem {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.history.last().map(|record| &record.progress),
            Some(ControlProgress::Applied { .. } | ControlProgress::Failed { .. })
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlBatchRecord {
    pub id: Uuid,
    pub sender: SenderIdentity,
    pub submitted: CommitReceipt,
    pub items: Vec<ControlBatchItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlBatchUpdate {
    pub batch_id: Uuid,
    pub sender: SenderIdentity,
    pub target: FlowRef,
    pub progress: ControlProgress,
}

#[derive(Clone, Default)]
pub(super) struct ControlBatches(HashMap<(SenderIdentity, Uuid), ControlBatchRecord>);

impl ControlBatches {
    fn previous(&self, request: &ControlBatchRequest) -> io::Result<Option<&ControlBatchRecord>> {
        let record = self.0.get(&(request.sender.clone(), request.id));
        if let Some(record) = record
            && (record.items.len() != request.commands.len()
                || !record
                    .items
                    .iter()
                    .zip(&request.commands)
                    .all(|(item, request)| &item.request == request))
        {
            return Err(invalid(
                "control batch identity conflicts with recorded commands",
            ));
        }
        Ok(record)
    }

    pub(super) fn submit(
        &mut self,
        request: ControlBatchRequest,
        receipt: CommitReceipt,
    ) -> io::Result<()> {
        let mut targets = HashSet::new();
        let invalid_sender = match &request.sender {
            SenderIdentity::User(id) | SenderIdentity::Host(id) => id.is_nil(),
            SenderIdentity::Flow(flow) => {
                flow.session_id.0.is_nil() || flow.instance_id.0.0.is_nil()
            }
        };
        if request.id.is_nil()
            || invalid_sender
            || request.commands.is_empty()
            || self.previous(&request)?.is_some()
            || request.commands.iter().any(|command| {
                command.sender != request.sender
                    || command.id.is_nil()
                    || command.pause_key.0.is_nil()
                    || command.target.session_id.0.is_nil()
                    || command.target.instance_id.0.0.is_nil()
                    || !targets.insert(command.target.clone())
            })
        {
            return Err(invalid("invalid or duplicate control batch"));
        }
        self.0.insert(
            (request.sender.clone(), request.id),
            ControlBatchRecord {
                id: request.id,
                sender: request.sender,
                submitted: receipt,
                items: request
                    .commands
                    .into_iter()
                    .map(|request| ControlBatchItem {
                        request,
                        history: Vec::new(),
                    })
                    .collect(),
            },
        );
        Ok(())
    }

    fn item(&self, update: &ControlBatchUpdate) -> io::Result<&ControlBatchItem> {
        self.0
            .get(&(update.sender.clone(), update.batch_id))
            .and_then(|record| {
                record
                    .items
                    .iter()
                    .find(|item| item.request.target == update.target)
            })
            .ok_or_else(|| invalid("control batch target does not exist"))
    }

    fn previous_progress(&self, update: &ControlBatchUpdate) -> io::Result<Option<&CommitReceipt>> {
        Ok(self
            .item(update)?
            .history
            .iter()
            .find(|record| record.progress == update.progress)
            .map(|record| &record.recorded))
    }

    pub(super) fn advance(
        &mut self,
        update: ControlBatchUpdate,
        recorded: CommitReceipt,
    ) -> io::Result<()> {
        let item = self.item(&update)?;
        if item.is_terminal() || self.previous_progress(&update)?.is_some() {
            return Err(invalid("control batch target progress is already recorded"));
        }
        if let ControlProgress::Accepted { receipt } | ControlProgress::Applied { receipt } =
            &update.progress
            && (receipt.journal_id != update.target.instance_id.0.0 || receipt.sequence == 0)
        {
            return Err(invalid(
                "control progress receipt belongs to another target",
            ));
        }
        match (
            &update.progress,
            item.history.last().map(|record| &record.progress),
        ) {
            (ControlProgress::Accepted { .. }, None) => (),
            (
                ControlProgress::Applied { receipt },
                Some(ControlProgress::Accepted { receipt: accepted }),
            ) if receipt.sequence > accepted.sequence || receipt == accepted => (),
            (ControlProgress::Failed { reason }, _) if !reason.trim().is_empty() => (),
            _ => return Err(invalid("invalid control batch progress transition")),
        }
        let item = self
            .0
            .get_mut(&(update.sender, update.batch_id))
            .unwrap()
            .items
            .iter_mut()
            .find(|item| item.request.target == update.target)
            .unwrap();
        item.history.push(ControlProgressRecord {
            progress: update.progress,
            recorded,
        });
        Ok(())
    }
}

impl ProfileStore {
    /// Record the entire target snapshot before delivery. The caller authenticates and authorizes admission, including retries.
    pub fn submit_controls(
        &mut self,
        request: ControlBatchRequest,
    ) -> io::Result<ControlBatchRecord> {
        if let Some(record) = self.projection.controls.previous(&request)? {
            return Ok(record.clone());
        }
        let key = (request.sender.clone(), request.id);
        self.commit(Fact::ControlsSubmitted(request))?;
        Ok(self.projection.controls.0[&key].clone())
    }

    pub fn control_batch(&self, sender: &SenderIdentity, id: Uuid) -> Option<&ControlBatchRecord> {
        self.projection.controls.0.get(&(sender.clone(), id))
    }

    /// Unfinished batches in acceptance order, including targets waiting for execution cleanup.
    pub fn pending_controls(&self) -> Vec<&ControlBatchRecord> {
        let mut pending: Vec<_> = self
            .projection
            .controls
            .0
            .values()
            .filter(|record| record.items.iter().any(|item| !item.is_terminal()))
            .collect();
        pending.sort_by_key(|record| record.submitted.sequence);
        pending
    }

    /// Record trusted delivery or execution evidence. Retrying a past progress report returns its original receipt without rolling back later progress.
    pub fn advance_control(&mut self, update: ControlBatchUpdate) -> io::Result<CommitReceipt> {
        if let Some(receipt) = self.projection.controls.previous_progress(&update)? {
            return Ok(receipt.clone());
        }
        self.commit(Fact::ControlProgressed(update))
    }
}
