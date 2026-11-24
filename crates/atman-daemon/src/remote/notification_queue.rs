use std::{io, path::Path, sync::Mutex};

use atman_proto::{SessionId, flows::FlowAddress};
use atman_runtime::{
    event_log::journal::CommitReceipt,
    flow_store::{
        notification::{FlowNotification, FlowNotificationKind},
        position::FlowPage,
    },
};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::devices::{NotificationRecipient, Permission};

const RETENTION: TimeDelta = TimeDelta::hours(24);
const MAX_PER_DEVICE: i64 = 256;
const MAX_RETAINED: i64 = 32_768;

pub(super) struct Queue(Mutex<Connection>);

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum NoticeKind {
    TurnCompleted,
    ApprovalRequested,
}

impl From<&FlowNotificationKind> for NoticeKind {
    fn from(kind: &FlowNotificationKind) -> Self {
        match kind {
            FlowNotificationKind::TurnCompleted => Self::TurnCompleted,
            FlowNotificationKind::ApprovalRequested { .. } => Self::ApprovalRequested,
        }
    }
}

#[derive(Serialize)]
pub(super) struct Destination {
    flow: FlowAddress,
    kind: NoticeKind,
}

pub(super) struct PendingDelivery {
    pub id: Uuid,
    pub device: Uuid,
    pub enrollment: Uuid,
    pub flow: FlowAddress,
    pub kind: FlowNotificationKind,
    pub expires_at: DateTime<Utc>,
    attempts: u8,
}

#[derive(Clone, Copy)]
pub(super) enum Attempt {
    Accepted,
    Obsolete,
    Retry,
}

impl Queue {
    pub(super) fn open(directory: &Path) -> io::Result<Self> {
        let file = directory.canonicalize()?.join("notifications.sqlite3");
        let flags = OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let db = Connection::open_with_flags(file, flags).map_err(io::Error::other)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS sources (
                session TEXT NOT NULL,
                flow TEXT NOT NULL,
                position TEXT NOT NULL,
                updated INTEGER NOT NULL,
                PRIMARY KEY (session, flow)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS notifications (
                id TEXT PRIMARY KEY,
                device TEXT NOT NULL,
                enrollment TEXT NOT NULL,
                session TEXT NOT NULL,
                flow TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                digest TEXT NOT NULL,
                fact INTEGER NOT NULL,
                event INTEGER NOT NULL,
                kind TEXT NOT NULL,
                expires INTEGER NOT NULL,
                UNIQUE (device, session, flow, sequence, digest, fact, event)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS notification_expiry ON notifications (expires);
             CREATE INDEX IF NOT EXISTS notification_device ON notifications (device);
             CREATE TABLE IF NOT EXISTS notification_delivery (
                id TEXT PRIMARY KEY REFERENCES notifications (id) ON DELETE CASCADE,
                attempts INTEGER NOT NULL,
                retry_at INTEGER NOT NULL,
                finished TEXT
             ) WITHOUT ROWID;",
        )
        .map_err(io::Error::other)?;
        Ok(Self(Mutex::new(db)))
    }

    pub(super) fn checkpoint(&self, flow: &FlowAddress) -> io::Result<Option<CommitReceipt>> {
        checkpoint(&self.0.lock().unwrap(), flow)
    }

    /// Atomically install the records and the cursor that consumed the entire source page.
    pub(super) fn record(
        &self,
        flow: &FlowAddress,
        after: Option<&CommitReceipt>,
        page: &FlowPage<FlowNotification>,
        recipients: &[NotificationRecipient],
        now: DateTime<Utc>,
    ) -> io::Result<()> {
        let mut previous = after;
        for batch in &page.batches {
            let receipt = batch.receipt.require_durable()?;
            if receipt.journal_id != flow.instance_id
                || previous.map_or(Some(1), |previous| previous.sequence.checked_add(1))
                    != Some(receipt.sequence)
                || !valid_digest(&receipt.digest)
            {
                return Err(invalid_page());
            }
            previous = Some(receipt);
        }
        if page
            .next
            .as_ref()
            .map(|position| position.require_durable())
            .transpose()?
            != previous
        {
            return Err(invalid_page());
        }
        let Some(next) = previous.filter(|_| !page.batches.is_empty()) else {
            return Ok(());
        };
        let recipients: Vec<_> = recipients
            .iter()
            .filter(|recipient| recipient.scope.session_id == flow.session_id.0)
            .collect();
        let db = &mut *self.0.lock().unwrap();
        let txn = db.transaction().map_err(io::Error::other)?;
        if checkpoint(&txn, flow)?.as_ref() != after {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "notification checkpoint changed",
            ));
        }
        let now_ms = now.timestamp_millis();
        txn.execute("DELETE FROM notifications WHERE expires <= ?1", [now_ms])
            .map_err(io::Error::other)?;
        let session = flow.session_id.0.to_string();
        let source = flow.instance_id.to_string();
        let mut insert = txn.prepare_cached(
            "INSERT INTO notifications (id, device, enrollment, session, flow, sequence, digest, fact, event, kind, expires)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (device, session, flow, sequence, digest, fact, event) DO NOTHING",
        ).map_err(io::Error::other)?;
        for batch in &page.batches {
            let receipt = batch.receipt.require_durable()?;
            for event in &batch.facts {
                let expires = event.at.min(now) + RETENTION;
                if expires <= now {
                    continue;
                }
                let approval = matches!(event.kind, FlowNotificationKind::ApprovalRequested { .. });
                for recipient in &recipients {
                    if recipient.enrollment.enabled_at > event.at
                        || (approval && !recipient.scope.permission.allows(Permission::Approve))
                    {
                        continue;
                    }
                    let kind = serde_json::to_string(&event.kind)?;
                    let event_index = event
                        .event_index
                        .map(i64::try_from)
                        .transpose()
                        .map_err(io::Error::other)?
                        .unwrap_or(-1);
                    insert
                        .execute(params![
                            Uuid::new_v4().to_string(),
                            recipient.device_id.to_string(),
                            recipient.enrollment.id.to_string(),
                            session,
                            source,
                            receipt.sequence,
                            receipt.digest,
                            event.fact_index,
                            event_index,
                            kind,
                            expires.timestamp_millis(),
                        ])
                        .map_err(io::Error::other)?;
                }
            }
        }
        drop(insert);
        for recipient in recipients {
            let count: i64 = txn
                .query_row(
                    "SELECT COUNT(*) FROM notifications WHERE device = ?1",
                    [recipient.device_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(io::Error::other)?;
            if count > MAX_PER_DEVICE {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "notification capacity reached",
                ));
            }
        }
        let total: i64 = txn
            .query_row("SELECT COUNT(*) FROM notifications", [], |row| row.get(0))
            .map_err(io::Error::other)?;
        if total > MAX_RETAINED {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "notification capacity reached",
            ));
        }
        txn.execute(
            "INSERT INTO sources (session, flow, position, updated) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session, flow) DO UPDATE SET position = excluded.position, updated = excluded.updated",
            params![session, source, serde_json::to_string(next)?, now_ms],
        )
        .map_err(io::Error::other)?;
        txn.commit().map_err(io::Error::other)
    }

    pub(super) fn prune(&self, now: DateTime<Utc>) -> io::Result<()> {
        let db = &mut *self.0.lock().unwrap();
        let txn = db.transaction().map_err(io::Error::other)?;
        txn.execute(
            "DELETE FROM notifications WHERE expires <= ?1",
            [now.timestamp_millis()],
        )
        .map_err(io::Error::other)?;
        txn.execute(
            "DELETE FROM sources WHERE updated <= ?1",
            [(now - RETENTION * 2).timestamp_millis()],
        )
        .map_err(io::Error::other)?;
        txn.commit().map_err(io::Error::other)
    }

    pub(super) fn pending(&self, now: DateTime<Utc>) -> io::Result<Vec<PendingDelivery>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare_cached(
            "SELECT n.id, n.device, n.enrollment, n.session, n.flow, n.kind, n.expires, COALESCE(d.attempts, 0)
             FROM notifications n LEFT JOIN notification_delivery d ON n.id = d.id
             WHERE n.expires > ?1 AND d.finished IS NULL AND COALESCE(d.retry_at, 0) <= ?1
             ORDER BY COALESCE(d.retry_at, 0), n.expires, n.id LIMIT 16"
        ).map_err(io::Error::other)?;
        let mut rows = query
            .query([now.timestamp_millis()])
            .map_err(io::Error::other)?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next().map_err(io::Error::other)? {
            let id = |column| {
                row.get::<_, String>(column)
                    .map_err(io::Error::other)?
                    .parse::<Uuid>()
                    .map_err(io::Error::other)
            };
            pending.push(PendingDelivery {
                id: id(0)?,
                device: id(1)?,
                enrollment: id(2)?,
                flow: FlowAddress {
                    session_id: SessionId(id(3)?),
                    instance_id: id(4)?,
                },
                kind: serde_json::from_str(&row.get::<_, String>(5).map_err(io::Error::other)?)?,
                expires_at: DateTime::from_timestamp_millis(row.get(6).map_err(io::Error::other)?)
                    .ok_or_else(|| io::Error::other("invalid notification expiry"))?,
                attempts: row.get(7).map_err(io::Error::other)?,
            });
        }
        Ok(pending)
    }

    pub(super) fn complete(
        &self,
        pending: &PendingDelivery,
        attempt: Attempt,
        now: DateTime<Utc>,
    ) -> io::Result<()> {
        let attempts = pending.attempts.saturating_add(1);
        let retry = now + TimeDelta::seconds(1 << pending.attempts.min(8));
        let status = match attempt {
            Attempt::Accepted => Some("accepted"),
            Attempt::Obsolete => Some("obsolete"),
            Attempt::Retry => None,
        };
        self.0.lock().unwrap().execute(
            "INSERT INTO notification_delivery (id, attempts, retry_at, finished)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM notifications WHERE id = ?1)
             ON CONFLICT (id) DO UPDATE SET attempts = excluded.attempts, retry_at = excluded.retry_at,
                 finished = excluded.finished WHERE notification_delivery.finished IS NULL",
            params![pending.id.to_string(), attempts, retry.timestamp_millis(), status],
        ).map_err(io::Error::other)?;
        Ok(())
    }

    pub(super) fn destination(
        &self,
        id: Uuid,
        recipient: Uuid,
        session: Uuid,
        permission: Permission,
        now: DateTime<Utc>,
    ) -> io::Result<Option<Destination>> {
        let row: Option<(String, String)> = self.0.lock().unwrap().query_row(
            "SELECT flow, kind FROM notifications WHERE id = ?1 AND device = ?2 AND session = ?3 AND expires > ?4",
            params![id.to_string(), recipient.to_string(), session.to_string(), now.timestamp_millis()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(io::Error::other)?;
        let Some((flow, kind)) = row else {
            return Ok(None);
        };
        let kind = NoticeKind::from(&serde_json::from_str::<FlowNotificationKind>(&kind)?);
        if matches!(kind, NoticeKind::ApprovalRequested) && !permission.allows(Permission::Approve)
        {
            return Ok(None);
        }
        let instance_id = flow.parse().map_err(io::Error::other)?;
        Ok(Some(Destination {
            flow: FlowAddress {
                session_id: SessionId(session),
                instance_id,
            },
            kind,
        }))
    }
}

fn checkpoint(db: &Connection, flow: &FlowAddress) -> io::Result<Option<CommitReceipt>> {
    let value: Option<String> = db
        .query_row(
            "SELECT position FROM sources WHERE session = ?1 AND flow = ?2",
            params![flow.session_id.0.to_string(), flow.instance_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(io::Error::other)?;
    value
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(Into::into)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn invalid_page() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid notification source page",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{devices::Scope, notification_subscription::Enrollment};
    use atman_runtime::{flow_store::position::FlowBatch, permission::PermissionRequestId};

    #[test]
    fn notification_transaction_preserves_scope_checkpoint_and_expiry_through_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let queue = Queue::open(directory.path()).unwrap();
        let now = Utc::now();
        let flow = FlowAddress {
            session_id: SessionId(Uuid::new_v4()),
            instance_id: Uuid::new_v4(),
        };
        let device = |permission| NotificationRecipient {
            device_id: Uuid::new_v4(),
            scope: Scope {
                session_id: flow.session_id.0,
                permission,
            },
            enrollment: Enrollment {
                id: Uuid::new_v4(),
                enabled_at: now - TimeDelta::seconds(1),
            },
        };
        let read = device(Permission::Read);
        let approve = device(Permission::Approve);
        let receipt = CommitReceipt {
            journal_id: flow.instance_id,
            sequence: 1,
            digest: "a".repeat(64),
        };
        let event = |event_index, kind| FlowNotification {
            fact_index: 0,
            event_index,
            at: now,
            kind,
        };
        let page = FlowPage {
            batches: vec![FlowBatch {
                receipt: receipt.clone().into(),
                facts: vec![
                    event(None, FlowNotificationKind::TurnCompleted),
                    event(
                        Some(0),
                        FlowNotificationKind::ApprovalRequested {
                            request_id: PermissionRequestId::now(),
                            revision: 2,
                        },
                    ),
                ],
            }],
            next: Some(receipt.clone().into()),
            has_more: false,
        };
        queue
            .record(&flow, None, &page, &[read, approve], now)
            .unwrap();
        let id_for = |recipient: NotificationRecipient| -> Uuid {
            let origin: String = queue
                .0
                .lock()
                .unwrap()
                .query_row(
                    "SELECT id FROM notifications WHERE device = ?1 AND event = 0",
                    [recipient.device_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            Uuid::parse_str(&origin).unwrap()
        };
        let approval_id = id_for(approve);
        let pending = queue
            .pending(now)
            .unwrap()
            .into_iter()
            .find(|pending| pending.id == approval_id)
            .unwrap();
        queue.complete(&pending, Attempt::Retry, now).unwrap();
        assert!(
            queue
                .pending(now)
                .unwrap()
                .iter()
                .all(|item| item.id != approval_id)
        );
        let overflow = CommitReceipt {
            sequence: 2,
            digest: "b".repeat(64),
            ..receipt.clone()
        };
        let overflowing_page = FlowPage {
            batches: vec![FlowBatch {
                receipt: overflow.clone().into(),
                facts: (0..MAX_PER_DEVICE as usize)
                    .map(|fact_index| FlowNotification {
                        fact_index,
                        ..event(None, FlowNotificationKind::TurnCompleted)
                    })
                    .collect(),
            }],
            next: Some(overflow.into()),
            has_more: false,
        };
        assert_eq!(
            queue
                .record(
                    &flow,
                    Some(&receipt),
                    &overflowing_page,
                    &[read, approve],
                    now
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        assert_eq!(queue.checkpoint(&flow).unwrap(), Some(receipt.clone()));
        assert!(
            queue
                .record(&flow, None, &page, &[read, approve], now)
                .is_err()
        );
        assert_eq!(id_for(approve), approval_id);
        drop(queue);

        let queue = Queue::open(directory.path()).unwrap();
        assert_eq!(queue.checkpoint(&flow).unwrap(), Some(receipt));
        let retry_at = now + TimeDelta::seconds(1);
        let retry = queue
            .pending(retry_at)
            .unwrap()
            .into_iter()
            .find(|item| item.id == approval_id)
            .unwrap();
        assert_eq!(retry.attempts, 1);
        queue.complete(&retry, Attempt::Accepted, retry_at).unwrap();
        assert!(
            queue
                .pending(retry_at)
                .unwrap()
                .iter()
                .all(|item| item.id != approval_id)
        );
        let lookup = |id, recipient: NotificationRecipient, when| {
            queue
                .destination(
                    id,
                    recipient.device_id,
                    recipient.scope.session_id,
                    recipient.scope.permission,
                    when,
                )
                .unwrap()
        };
        assert!(matches!(
            lookup(approval_id, approve, now).unwrap().kind,
            NoticeKind::ApprovalRequested
        ));
        assert!(lookup(approval_id, read, now).is_none());
        assert!(
            lookup(
                approval_id,
                NotificationRecipient {
                    scope: Scope {
                        permission: Permission::Read,
                        ..approve.scope
                    },
                    ..approve
                },
                now
            )
            .is_none()
        );
        assert!(lookup(approval_id, approve, now + RETENTION).is_none());
        let count: i64 = queue
            .0
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM notifications", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
        queue.prune(now + RETENTION).unwrap();
        let attempts: i64 = queue
            .0
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM notification_delivery", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(attempts, 0);
    }
}
