//! Append-only conversation ancestry metadata, independent of execution and model compaction.

use std::{collections::HashSet, io};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowRef, FlowStore, invalid,
    source_capture::{ConversationBranchId, SourceSelection},
};
use crate::event_log::journal::CommitReceipt;

mod inspection;

/// Head selection version, distinct from execution revisions and journal sequence numbers.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct HistoryHeadVersion(u64);

impl HistoryHeadVersion {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryHead {
    pub branch: ConversationBranchId,
    pub version: HistoryHeadVersion,
}

impl HistoryHead {
    pub fn initial(flow: &FlowRef, journal: Uuid) -> Self {
        Self {
            branch: ConversationBranchId::initial(flow, journal),
            version: HistoryHeadVersion::default(),
        }
    }
}

/// Creation and selection share one fact and one journal acknowledgement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistorySelection {
    Create {
        branch: ConversationBranchId,
        parent: SourceSelection,
    },
    Restore {
        branch: ConversationBranchId,
    },
}

/// Metadata for an owner-authorized operation; this record grants no execution or write authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryHeadChange {
    pub operation: Uuid,
    pub expected: HistoryHead,
    pub selection: HistorySelection,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConversationBranch {
    pub id: ConversationBranchId,
    pub parent: Option<SourceSelection>,
    pub created: CommitReceipt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommittedHistoryHeadChange {
    pub change: HistoryHeadChange,
    pub head: HistoryHead,
    pub receipt: CommitReceipt,
}

/// Inclusive ordinal range in the verified source journal, not an independently valid cursor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConversationSpan {
    pub branch: ConversationBranchId,
    pub first_sequence: u64,
    pub last_sequence: u64,
}

/// Read-only branch metadata. Actual effects and lifecycle facts remain in the original journal.
#[derive(Clone, Debug, Serialize)]
pub struct ConversationHistory {
    pub flow: FlowRef,
    pub head: HistoryHead,
    pub branches: Vec<ConversationBranch>,
    pub changes: Vec<CommittedHistoryHeadChange>,
    pub observed: CommitReceipt,
    pub trailing_bytes: u64,
}

impl ConversationHistory {
    /// Membership ranges for one retained branch, including its original inherited prefix.
    pub fn ancestry(&self, branch: ConversationBranchId) -> io::Result<Vec<ConversationSpan>> {
        let mut spans = Vec::new();
        let mut selected = branch;
        let mut through = self.observed.sequence;
        // Construction only accepts older parents. The bound also protects a modified local DTO.
        for _ in 0..self.branches.len() {
            let record = self.branch(selected)?;
            let mut owner = self.branches[0].id;
            let mut first = 1;
            for change in &self.changes {
                if change.receipt.sequence > through {
                    break;
                }
                if owner == selected && first < change.receipt.sequence {
                    spans.push(ConversationSpan {
                        branch: selected,
                        first_sequence: first,
                        last_sequence: change.receipt.sequence - 1,
                    });
                }
                owner = change.head.branch;
                first = change.receipt.sequence;
            }
            if owner == selected && first <= through {
                spans.push(ConversationSpan {
                    branch: selected,
                    first_sequence: first,
                    last_sequence: through,
                });
            }
            let Some(parent) = &record.parent else {
                spans.sort_unstable_by_key(|span| span.first_sequence);
                return Ok(spans);
            };
            through = through.min(parent.cursor.sequence);
            selected = parent.branch;
        }
        Err(invalid("conversation ancestry contains a cycle"))
    }

    fn branch(&self, id: ConversationBranchId) -> io::Result<&ConversationBranch> {
        self.branches
            .iter()
            .find(|branch| branch.id == id)
            .ok_or_else(|| invalid("conversation branch is not recorded"))
    }

    fn contains(&self, mut branch: ConversationBranchId, sequence: u64) -> io::Result<bool> {
        let index = self
            .changes
            .partition_point(|change| change.receipt.sequence <= sequence);
        let owner = index
            .checked_sub(1)
            .map_or(self.branches[0].id, |index| self.changes[index].head.branch);
        for _ in 0..self.branches.len() {
            if owner == branch {
                return Ok(true);
            }
            let Some(parent) = &self.branch(branch)?.parent else {
                return Ok(false);
            };
            if sequence > parent.cursor.sequence {
                return Ok(false);
            }
            branch = parent.branch;
        }
        Err(invalid("conversation ancestry contains a cycle"))
    }
}

struct Projection {
    history: ConversationHistory,
    operations: HashSet<Uuid>,
}

impl Projection {
    fn new(flow: FlowRef, created: CommitReceipt) -> io::Result<Self> {
        if created.journal_id != flow.instance_id.0.0 || created.sequence != 1 {
            return Err(invalid(
                "conversation history requires its original creation receipt",
            ));
        }
        let head = HistoryHead::initial(&flow, created.journal_id);
        Ok(Self {
            history: ConversationHistory {
                flow,
                branches: vec![ConversationBranch {
                    id: head.branch,
                    parent: None,
                    created: created.clone(),
                }],
                head,
                changes: Vec::new(),
                observed: created,
                trailing_bytes: 0,
            },
            operations: HashSet::new(),
        })
    }

    fn apply(
        &mut self,
        change: HistoryHeadChange,
        receipt: CommitReceipt,
        mut validate: impl FnMut(&CommitReceipt) -> io::Result<()>,
    ) -> io::Result<()> {
        validate(&receipt)?;
        if receipt.journal_id != self.history.observed.journal_id
            || receipt.sequence <= self.history.observed.sequence
            || change.expected != self.history.head
            || change.operation.is_nil()
            || self.operations.contains(&change.operation)
        {
            return Err(invalid("stale or duplicated history head change"));
        }
        let version = self
            .history
            .head
            .version
            .0
            .checked_add(1)
            .ok_or_else(|| invalid("history head version exhausted"))?;
        let (branch, created) = match &change.selection {
            HistorySelection::Create { branch, parent } => {
                validate(&parent.cursor)?;
                if branch.is_nil()
                    || self
                        .history
                        .branches
                        .iter()
                        .any(|record| record.id == *branch)
                    || parent.flow != self.history.flow
                    || parent.branch != self.history.head.branch
                    || parent.cursor.journal_id != receipt.journal_id
                    || parent.cursor.sequence >= receipt.sequence
                    || !self
                        .history
                        .contains(parent.branch, parent.cursor.sequence)?
                {
                    return Err(invalid("invalid conversation branch ancestry"));
                }
                (
                    *branch,
                    Some(ConversationBranch {
                        id: *branch,
                        parent: Some(parent.clone()),
                        created: receipt.clone(),
                    }),
                )
            }
            HistorySelection::Restore { branch } => {
                self.history.branch(*branch)?;
                if *branch == self.history.head.branch {
                    return Err(invalid("conversation branch is already selected"));
                }
                (*branch, None)
            }
        };
        // Nothing observable changes until every identity and ancestry check has passed.
        if let Some(created) = created {
            self.history.branches.push(created);
        }
        self.operations.insert(change.operation);
        let head = HistoryHead {
            branch,
            version: HistoryHeadVersion(version),
        };
        self.history.changes.push(CommittedHistoryHeadChange {
            change,
            head: head.clone(),
            receipt: receipt.clone(),
        });
        self.history.head = head;
        self.history.observed = receipt;
        Ok(())
    }
}

impl FlowStore {
    /// Unbranched owners retain their deterministic initial head without migrating old journals.
    pub fn history_head(&self) -> HistoryHead {
        HistoryHead::initial(self.state().address(), self.cursor().journal_id())
    }
}

pub(crate) fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "conversation branch execution and transcript projection are not supported by this reader",
    )
}

#[cfg(test)]
mod tests;
