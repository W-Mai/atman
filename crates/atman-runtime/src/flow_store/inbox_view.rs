//! Read-only delivery history at a validated journal boundary.

use std::io;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowInstanceId, FlowRef, SessionInspection, inbox::InboxEntry, invalid, outbox::SendRecord,
};
use crate::event_log::journal::CommitReceipt;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxSide {
    Received,
    Sent,
}

/// Pagination follows original acceptance order, independent of editable queue order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InboxHistoryCursor {
    pub owner: FlowRef,
    pub side: InboxSide,
    pub id: Uuid,
    pub receipt: CommitReceipt,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "side", content = "record", rename_all = "snake_case")]
pub enum InboxHistoryItem {
    Received(Box<InboxEntry>),
    Sent(Box<SendRecord>),
}

/// Each page is a fresh recorded snapshot. Input commitment and receiver acceptance are not processing completion.
#[derive(Debug, Serialize)]
pub struct InboxHistoryPage {
    pub owner: FlowRef,
    pub side: InboxSide,
    pub cursor: CommitReceipt,
    pub trailing_bytes: u64,
    pub items: Vec<InboxHistoryItem>,
    pub total: usize,
    pub next: Option<InboxHistoryCursor>,
}

impl SessionInspection {
    /// Read accepted and outgoing delivery records without acquiring a writer, recovering a turn, repairing a tail, or consuming input. Cursors from another direction, Flow, or journal are rejected.
    pub fn inspect_inbox(
        &self,
        instance: &FlowInstanceId,
        side: InboxSide,
        before: Option<&InboxHistoryCursor>,
        limit: usize,
    ) -> io::Result<InboxHistoryPage> {
        if !(1..=50).contains(&limit) {
            return Err(invalid("inbox history page limit must be between 1 and 50"));
        }
        let owner = FlowRef {
            session_id: self.projection.descriptor.id.into(),
            instance_id: instance.clone(),
        };
        if before.is_some_and(|cursor| cursor.owner != owner || cursor.side != side) {
            return Err(invalid("inbox history cursor belongs to another view"));
        }
        let mut recorded =
            super::read_registered_flow(&self.dir, &self.projection, instance, |_, _| Ok(()))?;
        let inbox = &recorded.replay.projection.as_ref().unwrap().2;
        let slice = match side {
            InboxSide::Received => page(
                inbox.entries(),
                before,
                limit,
                |entry| (entry.envelope.id, &entry.acceptance.receipt),
                |entry| {
                    inbox
                        .read_entry(entry.envelope.id, |receipt| {
                            recorded.reader.read_batch(receipt)
                        })?
                        .map(|body| InboxHistoryItem::Received(Box::new(body)))
                        .ok_or_else(|| invalid("inbox history entry is unavailable"))
                },
            )?,
            InboxSide::Sent => page(
                recorded.replay.outbox.records(),
                before,
                limit,
                |record| (record.request_id, &record.queued),
                |record| {
                    record
                        .read(recorded.reader.read_batch(&record.queued)?)
                        .map(|body| InboxHistoryItem::Sent(Box::new(body)))
                },
            )?,
        };
        Ok(InboxHistoryPage {
            next: slice.next.map(|(id, receipt)| InboxHistoryCursor {
                owner: owner.clone(),
                side,
                id,
                receipt,
            }),
            owner,
            side,
            cursor: recorded.journal.cursor.unwrap(),
            trailing_bytes: recorded.journal.trailing_bytes,
            items: slice.items,
            total: slice.total,
        })
    }
}

struct HistorySlice {
    items: Vec<InboxHistoryItem>,
    total: usize,
    next: Option<(Uuid, CommitReceipt)>,
}

fn page<T>(
    records: &[T],
    before: Option<&InboxHistoryCursor>,
    limit: usize,
    identity: impl Fn(&T) -> (Uuid, &CommitReceipt),
    project: impl FnMut(&T) -> io::Result<InboxHistoryItem>,
) -> io::Result<HistorySlice> {
    let end = match before {
        Some(cursor) => records
            .iter()
            .position(|record| {
                let (id, receipt) = identity(record);
                id == cursor.id && receipt == &cursor.receipt
            })
            .ok_or_else(|| invalid("inbox history cursor is unavailable or changed"))?,
        None => records.len(),
    };
    let start = end.saturating_sub(limit);
    let items = records[start..end]
        .iter()
        .rev()
        .map(project)
        .collect::<io::Result<_>>()?;
    let next = (start > 0).then(|| {
        let (id, receipt) = identity(&records[start]);
        (id, receipt.clone())
    });
    Ok(HistorySlice {
        items,
        total: records.len(),
        next,
    })
}
