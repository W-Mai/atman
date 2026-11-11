//! Publication positions distinguish durable history from host-local observations.

use std::io;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::event_log::journal::{CommitReceipt, CommittedBatch, JournalPage};

/// A complete publication boundary. Only `Durable` certifies synchronized history.
///
/// Durable positions keep their existing wire representation. Transient positions have a separate envelope and cannot deserialize as journal commit receipts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FlowPosition {
    Durable(CommitReceipt),
    Transient { transient: TransientPosition },
}

impl<'de> Deserialize<'de> for FlowPosition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Durable {
            journal_id: Uuid,
            sequence: u64,
            digest: String,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Transient {
            transient: TransientPosition,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Durable(Durable),
            Transient(Transient),
        }

        Ok(match Wire::deserialize(deserializer)? {
            Wire::Durable(receipt) => Self::Durable(CommitReceipt {
                journal_id: receipt.journal_id,
                sequence: receipt.sequence,
                digest: receipt.digest,
            }),
            Wire::Transient(receipt) => Self::Transient {
                transient: receipt.transient,
            },
        })
    }
}

/// An observation valid only in one host generation and one flow's memory history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransientPosition {
    pub host_generation: Uuid,
    pub flow_id: Uuid,
    pub sequence: u64,
    pub digest: String,
}

impl FlowPosition {
    /// Scope equality includes host generation for memory-only publications.
    pub fn same_history(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Durable(left), Self::Durable(right)) => left.journal_id == right.journal_id,
            (Self::Transient { transient: left }, Self::Transient { transient: right }) => {
                left.host_generation == right.host_generation && left.flow_id == right.flow_id
            }
            _ => false,
        }
    }

    pub fn journal_id(&self) -> Uuid {
        match self {
            Self::Durable(receipt) => receipt.journal_id,
            Self::Transient { transient } => transient.flow_id,
        }
    }

    pub fn sequence(&self) -> u64 {
        match self {
            Self::Durable(receipt) => receipt.sequence,
            Self::Transient { transient } => transient.sequence,
        }
    }

    pub fn digest(&self) -> &str {
        match self {
            Self::Durable(receipt) => &receipt.digest,
            Self::Transient { transient } => &transient.digest,
        }
    }

    /// Require the actual durable acknowledgement at an on-disk boundary.
    pub fn require_durable(&self) -> io::Result<&CommitReceipt> {
        match self {
            Self::Durable(receipt) => Ok(receipt),
            Self::Transient { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "transient flow position is not a durable journal receipt",
            )),
        }
    }
}

impl From<CommitReceipt> for FlowPosition {
    fn from(receipt: CommitReceipt) -> Self {
        Self::Durable(receipt)
    }
}

/// One complete flow publication, with explicit retention in its position.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FlowBatch<T> {
    pub receipt: FlowPosition,
    pub facts: Vec<T>,
}

impl<T> From<CommittedBatch<T>> for FlowBatch<T> {
    fn from(batch: CommittedBatch<T>) -> Self {
        Self {
            receipt: batch.receipt.into(),
            facts: batch.facts,
        }
    }
}

/// A bounded flow observation page, independent of its retention backend.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FlowPage<T> {
    pub batches: Vec<FlowBatch<T>>,
    pub next: Option<FlowPosition>,
    pub has_more: bool,
}

impl<T> From<JournalPage<T>> for FlowPage<T> {
    fn from(page: JournalPage<T>) -> Self {
        Self {
            batches: page.batches.into_iter().map(Into::into).collect(),
            next: page.next.map(Into::into),
            has_more: page.has_more,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_wire_is_unchanged_and_transient_positions_cannot_claim_durability() {
        let receipt = CommitReceipt {
            journal_id: Uuid::now_v7(),
            sequence: 3,
            digest: "digest".into(),
        };
        let position = FlowPosition::from(receipt.clone());
        let wire = serde_json::to_value(&receipt).unwrap();
        assert_eq!(serde_json::to_value(&position).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<FlowPosition>(wire).unwrap(),
            position
        );
        assert_eq!(position.require_durable().unwrap(), &receipt);

        let transient = FlowPosition::Transient {
            transient: TransientPosition {
                host_generation: Uuid::now_v7(),
                flow_id: receipt.journal_id,
                sequence: receipt.sequence,
                digest: receipt.digest,
            },
        };
        let wire = serde_json::to_value(&transient).unwrap();
        assert!(serde_json::from_value::<CommitReceipt>(wire.clone()).is_err());
        assert_eq!(
            serde_json::from_value::<FlowPosition>(wire).unwrap(),
            transient
        );
        assert!(transient.require_durable().is_err());
        assert_ne!(transient, position);
        assert!(!transient.same_history(&position));
        let mut mixed = serde_json::to_value(&position).unwrap();
        mixed["transient"] = serde_json::to_value(&transient).unwrap()["transient"].clone();
        assert!(serde_json::from_value::<FlowPosition>(mixed).is_err());
    }
}
