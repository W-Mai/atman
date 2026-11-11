//! Durable history-operation preparation with admission ownership independent of execution pauses.

use std::{collections::HashMap, io};

use atman_rt::{InstanceActivity, InstanceState, TurnLease};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowFact, FlowRef, FlowStore,
    branches::{HistoryHead, HistorySelection},
    control::{ControlAction, ControlRequest},
    goal::GoalVersion,
    inbox::SenderIdentity,
    invalid,
    position::FlowPosition,
    source_capture::{BoundarySource, ConversationBranchId, ConversationCutoff, SourceSelection},
};
use crate::{event::AtmanUuid, event_log::journal::CommitReceipt};

/// Reviewed identities remain fixed while the original invocation finishes cancellation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryOperationRequest {
    pub id: Uuid,
    pub sender: SenderIdentity,
    pub flow: FlowRef,
    pub expected_head: HistoryHead,
    pub execution_revision: u64,
    pub active: Option<TurnLease<AtmanUuid>>,
    pub goal: Option<GoalVersion>,
    pub queued: Vec<HistoryQueueItem>,
    pub target: HistoryOperationTarget,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryOperationTarget {
    Revert {
        observed: SourceSelection,
        cutoff: Box<ConversationCutoff>,
        branch: ConversationBranchId,
    },
    Restore {
        branch: ConversationBranchId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryQueueItem {
    pub message: Uuid,
    pub revision: FlowPosition,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryOperationEvent {
    Started {
        request: Box<HistoryOperationRequest>,
        selection: HistorySelection,
        at: DateTime<Utc>,
    },
    Aborted {
        sender: SenderIdentity,
        id: Uuid,
        at: DateTime<Utc>,
    },
    Stopping {
        sender: SenderIdentity,
        id: Uuid,
        control: Box<ControlRequest>,
        at: DateTime<Utc>,
    },
    ExecutionStopped {
        sender: SenderIdentity,
        id: Uuid,
        completion: Option<FlowPosition>,
        at: DateTime<Utc>,
    },
    Applied {
        sender: SenderIdentity,
        id: Uuid,
        head: HistoryHead,
        at: DateTime<Utc>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HistoryOperationStatus {
    Preparing,
    Stopping,
    Ready {
        receipt: FlowPosition,
        completion: Option<FlowPosition>,
    },
    Aborted {
        receipt: FlowPosition,
    },
    Applied {
        receipt: FlowPosition,
        head: HistoryHead,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HistoryCancellation {
    pub request: ControlRequest,
    pub receipt: FlowPosition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HistoryOperationRecord {
    pub request: HistoryOperationRequest,
    pub selection: HistorySelection,
    pub accepted: FlowPosition,
    pub cancellation: Option<HistoryCancellation>,
    pub status: HistoryOperationStatus,
}

#[derive(Default)]
pub(super) struct HistoryOperations {
    records: Vec<HistoryOperationRecord>,
    requests: HashMap<(SenderIdentity, Uuid), usize>,
    pending: Option<usize>,
}

impl HistoryOperations {
    pub(super) fn pending(&self) -> Option<&HistoryOperationRecord> {
        self.pending.map(|index| &self.records[index])
    }

    fn find(&self, sender: &SenderIdentity, id: Uuid) -> Option<&HistoryOperationRecord> {
        self.requests
            .get(&(sender.clone(), id))
            .map(|index| &self.records[*index])
    }

    fn previous(
        &self,
        request: &HistoryOperationRequest,
    ) -> io::Result<Option<&HistoryOperationRecord>> {
        match self.find(&request.sender, request.id) {
            Some(record) if &record.request != request => Err(invalid(
                "history operation identity conflicts with recorded data",
            )),
            record => Ok(record),
        }
    }

    pub(super) fn prepare(
        &self,
        request: &HistoryOperationRequest,
        state: &atman_rt::FlowInstance<AtmanUuid>,
        goal: Option<&crate::memory::goal::GoalRecord>,
        source: BoundarySource<'_>,
        history: &super::branches::ConversationHistory,
        previous: impl FnMut(&CommitReceipt) -> io::Result<Option<CommitReceipt>>,
    ) -> io::Result<HistorySelection> {
        if !matches!(request.sender, SenderIdentity::User(_)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "history operations require an authenticated user",
            ));
        }
        if request.id.is_nil() || self.previous(request)?.is_some() || self.pending.is_some() {
            return Err(invalid(
                "history operation identity is invalid or another operation is pending",
            ));
        }
        let InstanceState::Open(activity) = state.state() else {
            return Err(invalid("history operation requires an open native Flow"));
        };
        let active = match activity {
            InstanceActivity::Active(active) => Some(&active.lease),
            _ => None,
        };
        if &request.flow != state.address()
            || request.execution_revision != state.revision()
            || request.active.as_ref() != active
            || request.expected_head != *source.inbox.head()
            || request.goal != goal.map(GoalVersion::from)
            || !request
                .queued
                .iter()
                .map(|item| (&item.message, &item.revision))
                .eq(source
                    .inbox
                    .pending_entries()
                    .map(|entry| (&entry.envelope.id, &entry.revision)))
        {
            return Err(invalid("reviewed history operation state changed"));
        }
        match &request.target {
            HistoryOperationTarget::Revert {
                observed,
                cutoff,
                branch,
            } => {
                if branch.is_nil() || history.branches.iter().any(|item| item.id == *branch) {
                    return Err(invalid("history operation requires a new branch identity"));
                }
                let (parent, _) = source.resolve(observed, cutoff, history, previous)?;
                Ok(HistorySelection::Create {
                    branch: *branch,
                    parent,
                })
            }
            HistoryOperationTarget::Restore { branch } => {
                history.branch(*branch)?;
                if *branch == source.branch {
                    return Err(invalid("conversation branch is already selected"));
                }
                Ok(HistorySelection::Restore { branch: *branch })
            }
        }
    }

    pub(super) fn validate_abort(
        &self,
        sender: &SenderIdentity,
        id: Uuid,
    ) -> io::Result<&HistoryOperationRecord> {
        let record = self
            .find(sender, id)
            .ok_or_else(|| invalid("history operation is unavailable for this user"))?;
        if self.pending() != Some(record) {
            return Err(invalid("history operation no longer owns admission"));
        }
        Ok(record)
    }

    pub(super) fn apply(
        &mut self,
        event: HistoryOperationEvent,
        receipt: FlowPosition,
    ) -> HistoryOperationRecord {
        match event {
            HistoryOperationEvent::Started {
                request, selection, ..
            } => {
                let index = self.records.len();
                self.requests
                    .insert((request.sender.clone(), request.id), index);
                self.records.push(HistoryOperationRecord {
                    request: *request,
                    selection,
                    accepted: receipt,
                    cancellation: None,
                    status: HistoryOperationStatus::Preparing,
                });
                self.pending = Some(index);
                self.records[index].clone()
            }
            HistoryOperationEvent::Aborted { sender, id, .. } => {
                let index = self.requests[&(sender, id)];
                self.records[index].status = HistoryOperationStatus::Aborted { receipt };
                self.pending = None;
                self.records[index].clone()
            }
            HistoryOperationEvent::Stopping {
                sender,
                id,
                control,
                ..
            } => {
                let index = self.requests[&(sender, id)];
                self.records[index].cancellation = Some(HistoryCancellation {
                    request: *control,
                    receipt,
                });
                self.records[index].status = HistoryOperationStatus::Stopping;
                self.records[index].clone()
            }
            HistoryOperationEvent::ExecutionStopped {
                sender,
                id,
                completion,
                ..
            } => {
                let index = self.requests[&(sender, id)];
                self.records[index].status = HistoryOperationStatus::Ready {
                    receipt,
                    completion,
                };
                self.records[index].clone()
            }
            HistoryOperationEvent::Applied {
                sender, id, head, ..
            } => {
                let index = self.requests[&(sender, id)];
                self.records[index].status = HistoryOperationStatus::Applied { receipt, head };
                self.pending = None;
                self.records[index].clone()
            }
        }
    }
}

mod application;
pub(crate) use application::SelectedHistory;
mod review;
pub use review::{HistoryOperationReview, HistoryReviewRequest};
mod stopping;

impl FlowStore {
    /// Accepted preparation blocks new turn admission, not an already active invocation's cleanup.
    pub fn pending_history_operation(&self) -> Option<&HistoryOperationRecord> {
        self.history_operations.pending()
    }

    /// The host authorizes this user before querying its operation record.
    pub fn history_operation(
        &self,
        sender: &SenderIdentity,
        id: Uuid,
    ) -> Option<&HistoryOperationRecord> {
        self.history_operations.find(sender, id)
    }

    /// The host authenticates the sender and authorizes retries before accessing operation receipts.
    ///
    /// Preparation records the reviewed cutoff and owns an admission hold. It does not cancel execution or change the conversation head.
    pub fn begin_history_operation(
        &mut self,
        request: HistoryOperationRequest,
        authorize: impl FnOnce(&HistoryOperationRequest) -> io::Result<()>,
    ) -> io::Result<HistoryOperationRecord> {
        authorize(&request)?;
        if let Some(record) = self.history_operations.previous(&request)? {
            return Ok(record.clone());
        }
        let selection = self.prepare_history_operation(&request)?;
        let event = HistoryOperationEvent::Started {
            request: Box::new(request),
            selection,
            at: Utc::now(),
        };
        let receipt = self
            .journal
            .commit(&[FlowFact::HistoryOperation(Box::new(event.clone()))])?;
        self.inbox.invalidate();
        self.cursor = receipt.clone();
        Ok(self.history_operations.apply(event, receipt))
    }

    fn prepare_history_operation(
        &self,
        request: &HistoryOperationRequest,
    ) -> io::Result<HistorySelection> {
        self.cursor.require_durable()?;
        self.history_operations.prepare(
            request,
            &self.state,
            self.goals.current(),
            BoundarySource {
                flow: self.state.address(),
                branch: self.inbox.head().branch,
                inbox: &self.inbox,
                completions: &self.completions,
            },
            &self
                .branches
                .as_ref()
                .ok_or_else(|| invalid("history operation requires durable native ancestry"))?
                .history,
            |receipt| self.journal.previous_durable_receipt(receipt),
        )
    }

    /// Aborting preparation releases only this operation's admission hold; cancellation and pauses stay current.
    pub fn abort_history_operation(
        &mut self,
        sender: SenderIdentity,
        id: Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()>,
    ) -> io::Result<HistoryOperationRecord> {
        let record = self
            .history_operations
            .find(&sender, id)
            .ok_or_else(|| invalid("history operation is unavailable for this user"))?;
        authorize(record)?;
        if matches!(record.status, HistoryOperationStatus::Aborted { .. }) {
            return Ok(record.clone());
        }
        self.history_operations.validate_abort(&sender, id)?;
        let event = HistoryOperationEvent::Aborted {
            sender,
            id,
            at: Utc::now(),
        };
        let receipt = self
            .journal
            .commit(&[FlowFact::HistoryOperation(Box::new(event.clone()))])?;
        self.cursor = receipt.clone();
        Ok(self.history_operations.apply(event, receipt))
    }

    pub(crate) fn can_admit_turn(&self) -> bool {
        self.history_operations.pending().is_none() && self.state.can_start_turn()
    }

    pub(super) fn validate_turn_admission(&self) -> io::Result<()> {
        if self.history_operations.pending().is_some() {
            return Err(invalid("history operation is holding new turn admission"));
        }
        Ok(())
    }
}

impl super::SessionInspection {
    /// Read preparation or abort receipts without opening an execution owner or repairing the journal.
    pub fn history_operation(
        &self,
        flow: &super::FlowInstanceId,
        sender: &SenderIdentity,
        id: Uuid,
    ) -> io::Result<Option<HistoryOperationRecord>> {
        let recorded =
            super::read_registered_flow(&self.dir, &self.projection, flow, |_, _| Ok(()))?;
        Ok(recorded.replay.history_operations.find(sender, id).cloned())
    }
}

#[cfg(test)]
pub(crate) mod tests;
