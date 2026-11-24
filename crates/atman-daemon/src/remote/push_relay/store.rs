use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::Path,
};

use atman_runtime::platform::ExclusiveFileLease;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Envelope, Error, Registration, SEND_PREFIX, SendResult, Update};
use crate::remote::native_push::Subscription;

const REGISTRATION_AGE: i64 = 30 * 86_400;
const MAX_REGISTRATIONS: i64 = 8192;
const MAX_NOTICES: i64 = 65_536;
const MAX_PER_REGISTRATION: i64 = 128;

pub(super) struct Store {
    db: Connection,
    _writer: ExclusiveFileLease,
    watches: BTreeMap<Uuid, CancellationToken>,
    pruned: Option<i64>,
}

#[derive(Deserialize, Serialize)]
struct Binding {
    owner: [u8; 32],
    revision: Uuid,
    request_id: Uuid,
    request_digest: [u8; 32],
    active: Option<Enrollment>,
}

#[derive(Deserialize, Serialize)]
struct Enrollment {
    id: Uuid,
    sending: [u8; 32],
    subscription: Subscription,
}

pub(super) struct Job {
    pub id: Uuid,
    pub epoch: Uuid,
    pub notice: Envelope,
    pub expires: i64,
    attempts: u8,
}

pub(super) struct Grant {
    pub subscription: Subscription,
    pub revision: Uuid,
    pub expires: i64,
    pub changed: CancellationToken,
}

fn digest(secret: &[u8; 32]) -> [u8; 32] {
    *blake3::hash(secret).as_bytes()
}

fn equals(left: &[u8; 32], right: &[u8; 32]) -> bool {
    crate::http::constant_time_eq(left, right)
}

fn sending(owner: &[u8; 32], id: Uuid, epoch: Uuid) -> [u8; 32] {
    let mut key = blake3::Hasher::new_keyed(owner);
    key.update(b"atman push relay sender v1");
    key.update(id.as_bytes());
    key.update(epoch.as_bytes());
    *key.finalize().as_bytes()
}

impl Binding {
    fn view(&self, id: Uuid, owner: &[u8; 32], expires: i64) -> Registration {
        Registration {
            revision: self.revision,
            capability: self.active.as_ref().map(|epoch| {
                let secret = URL_SAFE_NO_PAD.encode(sending(owner, id, epoch.id));
                format!("{SEND_PREFIX}.{id}.{secret}")
            }),
            expires_at_unix: expires,
        }
    }
}

impl Store {
    pub(super) fn open(directory: &Path) -> anyhow::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if let Err(error) = builder.create(directory)
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(error.into());
        }
        let metadata = fs::symlink_metadata(directory)?;
        anyhow::ensure!(metadata.is_dir(), "relay state must be a directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
                "relay state must be private to its service user",
            );
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let writer = ExclusiveFileLease::try_acquire(options.open(directory.join("writer.lock"))?)?;
        let db = Connection::open_with_flags(
            directory.canonicalize()?.join("relay.sqlite3"),
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS registrations (
                id TEXT PRIMARY KEY, expires INTEGER NOT NULL, body TEXT NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS notices (
                registration TEXT REFERENCES registrations(id) ON DELETE CASCADE,
                epoch TEXT, id TEXT, kind TEXT NOT NULL, expires INTEGER NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0, retry_at INTEGER NOT NULL,
                done INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (registration, epoch, id)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS registration_expiry ON registrations(expires);
             CREATE INDEX IF NOT EXISTS relay_notice_expiry ON notices(expires);
             CREATE INDEX IF NOT EXISTS relay_notice_due ON notices(retry_at) WHERE done=0;",
        )?;
        Ok(Self {
            db,
            _writer: writer,
            watches: BTreeMap::new(),
            pruned: None,
        })
    }

    pub(super) fn registration(
        &self,
        id: Uuid,
        owner: &[u8; 32],
        now: i64,
    ) -> Result<Registration, Error> {
        let (binding, expiry) = read(&self.db, id)?.ok_or(Error::Missing)?;
        check_owner(&binding, expiry, owner, now)?;
        Ok(binding.view(id, owner, expiry))
    }

    pub(super) fn update(
        &mut self,
        id: Uuid,
        owner: &[u8; 32],
        update: Update,
        channel: String,
        now: i64,
    ) -> Result<Registration, Error> {
        let current = read(&self.db, id)?;
        let request_digest = *blake3::hash(&serde_json::to_vec(&update)?).as_bytes();
        if let Some((binding, expires)) = &current {
            check_owner(binding, *expires, owner, now)?;
            if binding.request_id == update.request_id {
                return if equals(&binding.request_digest, &request_digest) {
                    Ok(binding.view(id, owner, *expires))
                } else {
                    Err(Error::Conflict)
                };
            }
        } else {
            if update.device_token.is_none() {
                return Err(Error::Missing);
            }
            let count: i64 =
                self.db
                    .query_row("SELECT COUNT(*) FROM registrations", [], |row| row.get(0))?;
            if count >= MAX_REGISTRATIONS {
                return Err(Error::Busy);
            }
        }
        if update.expected_revision != current.as_ref().map(|(binding, _)| binding.revision) {
            return Err(Error::Conflict);
        }
        let old_epoch = current.and_then(|(binding, _)| binding.active);
        if update.device_token.is_none()
            && old_epoch
                .as_ref()
                .is_some_and(|epoch| epoch.subscription.platform != update.platform)
        {
            return Err(Error::Conflict);
        }
        let active = update.device_token.map(|device_token| {
            let same = old_epoch.as_ref().filter(|epoch| {
                epoch.subscription.platform == update.platform
                    && epoch.subscription.channel == channel
            });
            let epoch = same.map_or_else(Uuid::new_v4, |epoch| epoch.id);
            Enrollment {
                id: epoch,
                sending: digest(&sending(owner, id, epoch)),
                subscription: Subscription {
                    platform: update.platform,
                    channel,
                    device_token,
                },
            }
        });
        let binding = Binding {
            owner: digest(owner),
            revision: Uuid::new_v4(),
            request_id: update.request_id,
            request_digest,
            active,
        };
        let expires = now + REGISTRATION_AGE;
        self.db.execute(
            "INSERT INTO registrations (id,expires,body) VALUES (?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET expires=excluded.expires,body=excluded.body",
            params![id.to_string(), expires, serde_json::to_string(&binding)?],
        )?;
        self.changed(id);
        Ok(binding.view(id, owner, expires))
    }

    pub(super) fn publish(
        &mut self,
        id: Uuid,
        sending: &[u8; 32],
        notice: Envelope,
        now: i64,
        ttl: i64,
        supported: impl FnOnce(&Subscription) -> bool,
    ) -> Result<(), Error> {
        let (binding, expiry) = read(&self.db, id)?.ok_or(Error::Retired)?;
        let enrollment = binding
            .active
            .filter(|_| expiry > now)
            .ok_or(Error::Retired)?;
        if !equals(&enrollment.sending, &digest(sending)) {
            return Err(Error::Unauthorized);
        }
        if !supported(&enrollment.subscription) {
            return Err(Error::Retired);
        }
        let registration = id.to_string();
        let epoch = enrollment.id.to_string();
        let notice_id = notice.notification_id.to_string();
        let kind = serde_json::to_string(&notice.kind)?;
        let existing: Option<String> = self
            .db
            .query_row(
                "SELECT kind FROM notices WHERE registration=?1 AND epoch=?2 AND id=?3",
                params![registration, epoch, notice_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            return if kind == existing {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM notices", [], |row| row.get(0))?;
        let individual: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM notices WHERE registration=?1",
            [&registration],
            |row| row.get(0),
        )?;
        if count >= MAX_NOTICES || individual >= MAX_PER_REGISTRATION {
            return Err(Error::Busy);
        }
        self.db.execute(
            "INSERT INTO notices(registration,epoch,id,kind,expires,retry_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![registration, epoch, notice_id, kind, (now + ttl).min(expiry), now],
        )?;
        Ok(())
    }

    pub(super) fn admit(
        &mut self,
        id: Uuid,
        epoch: Uuid,
        now: i64,
    ) -> Result<Option<Grant>, Error> {
        let Some((binding, expires)) = read(&self.db, id)? else {
            return Ok(None);
        };
        let Some(active) = binding
            .active
            .filter(|active| active.id == epoch && expires > now)
        else {
            return Ok(None);
        };
        Ok(Some(Grant {
            subscription: active.subscription,
            revision: binding.revision,
            expires,
            changed: self.watches.entry(id).or_default().clone(),
        }))
    }

    pub(super) fn pending(&mut self, now: i64) -> Result<Vec<Job>, Error> {
        if self.pruned.is_none_or(|before| now - before >= 60) {
            let removed = {
                let tx = self.db.transaction()?;
                tx.execute("DELETE FROM notices WHERE expires<=?1", [now])?;
                let mut expired =
                    tx.prepare("DELETE FROM registrations WHERE expires<=?1 RETURNING id")?;
                let removed = expired
                    .query_map([now], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(expired);
                tx.commit()?;
                removed
            };
            for id in removed {
                self.changed(uuid(&id)?);
            }
            self.pruned = Some(now);
        }
        let mut query = self.db.prepare_cached(
            "SELECT registration,epoch,id,kind,expires,attempts FROM notices
             WHERE done=0 AND retry_at<=?1 AND expires>?1 ORDER BY retry_at LIMIT 16",
        )?;
        let mut rows = query.query([now])?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next()? {
            pending.push(Job {
                id: uuid(&row.get::<_, String>(0)?)?,
                epoch: uuid(&row.get::<_, String>(1)?)?,
                notice: Envelope {
                    notification_id: uuid(&row.get::<_, String>(2)?)?,
                    kind: serde_json::from_str(&row.get::<_, String>(3)?)?,
                },
                expires: row.get(4)?,
                attempts: row.get(5)?,
            });
        }
        Ok(pending)
    }

    pub(super) fn finish(
        &mut self,
        job: Job,
        mut result: SendResult,
        revision: Option<Uuid>,
        now: i64,
    ) -> Result<(), Error> {
        let tx = self.db.transaction()?;
        let mut retired = false;
        if matches!(result, SendResult::InvalidSubscription)
            && let Some(expected) = revision
            && let Some((mut binding, _)) = read(&tx, job.id)?
        {
            if binding.revision == expected {
                binding.active = None;
                binding.revision = Uuid::new_v4();
                binding.request_id = Uuid::nil();
                tx.execute(
                    "UPDATE registrations SET body=?1 WHERE id=?2",
                    params![serde_json::to_string(&binding)?, job.id.to_string()],
                )?;
                retired = true;
            } else {
                result = SendResult::Retry;
            }
        }
        let done = !matches!(result, SendResult::Retry);
        tx.execute(
            "UPDATE notices SET done=?1,attempts=?2,retry_at=?3 WHERE registration=?4 AND epoch=?5 AND id=?6",
            params![done, job.attempts.saturating_add(1), now + (1 << job.attempts.min(8)), job.id.to_string(),
                job.epoch.to_string(), job.notice.notification_id.to_string()],
        )?;
        tx.commit()?;
        if retired {
            self.changed(job.id);
        }
        Ok(())
    }

    fn changed(&mut self, id: Uuid) {
        if let Some(signal) = self.watches.remove(&id) {
            signal.cancel();
        }
    }
}

fn read(db: &Connection, id: Uuid) -> Result<Option<(Binding, i64)>, Error> {
    let row: Option<(String, i64)> = db
        .query_row(
            "SELECT body,expires FROM registrations WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(body, expires)| Ok((serde_json::from_str(&body)?, expires)))
        .transpose()
}

fn check_owner(binding: &Binding, expires: i64, owner: &[u8; 32], now: i64) -> Result<(), Error> {
    if !equals(&binding.owner, &digest(owner)) {
        return Err(Error::Unauthorized);
    }
    if expires <= now {
        return Err(Error::Retired);
    }
    Ok(())
}

fn uuid(value: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(value).map_err(|_| Error::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{native_push::Platform, notification_queue::NoticeKind};

    #[test]
    fn relay_reconciles_one_notice_after_restart_and_rejects_stale_retirement() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("relay");
        let mut store = Store::open(&directory).unwrap();
        let owner = [11; 32];
        let id = Uuid::new_v4();
        let channel = "fcm:fixture-project";
        let notice = Uuid::new_v4();
        let now = 1000;
        let update = |request_id, expected_revision, token: Option<&str>| Update {
            request_id,
            expected_revision,
            platform: Platform::Fcm,
            device_token: token.map(str::to_owned),
        };
        let request = Uuid::new_v4();
        let first = store
            .update(
                id,
                &owner,
                update(request, None, Some("fixture-original-token")),
                channel.into(),
                now,
            )
            .unwrap();
        let repeat = store
            .update(
                id,
                &owner,
                update(request, None, Some("fixture-original-token")),
                channel.into(),
                now + 1,
            )
            .unwrap();
        assert_eq!(first.revision, repeat.revision);
        assert_eq!(first.capability, repeat.capability);
        let sender = super::super::decode_secret(
            first
                .capability
                .as_ref()
                .unwrap()
                .rsplit_once('.')
                .unwrap()
                .1,
        )
        .unwrap();
        let draft = |kind| Envelope {
            notification_id: notice,
            kind,
        };
        let publish = |store: &mut Store, sender: &[u8; 32], kind, ttl| {
            store.publish(id, sender, draft(kind), now, ttl, |_| true)
        };
        assert!(matches!(
            publish(&mut store, &owner, NoticeKind::TurnCompleted, 10),
            Err(Error::Unauthorized)
        ));
        publish(&mut store, &sender, NoticeKind::TurnCompleted, 10).unwrap();
        drop(store);
        let mut store = Store::open(&directory).unwrap();
        publish(&mut store, &sender, NoticeKind::TurnCompleted, 60).unwrap();
        assert!(matches!(
            publish(&mut store, &sender, NoticeKind::ApprovalRequested, 20),
            Err(Error::Conflict)
        ));
        let mut jobs = store.pending(now).unwrap();
        assert_eq!(jobs.len(), 1);
        let job = jobs.pop().unwrap();
        assert_eq!(job.expires, now + 10);
        assert_eq!(job.notice.notification_id, notice);
        let old = store.admit(id, job.epoch, now).unwrap().unwrap();
        let next = store
            .update(
                id,
                &owner,
                update(
                    Uuid::new_v4(),
                    Some(first.revision),
                    Some("fixture-replacement-token"),
                ),
                channel.into(),
                now,
            )
            .unwrap();
        assert_eq!(next.capability, first.capability);
        assert!(old.changed.is_cancelled());
        store
            .finish(
                job,
                SendResult::InvalidSubscription,
                Some(old.revision),
                now,
            )
            .unwrap();
        let retry = store.pending(now + 1).unwrap().pop().unwrap();
        assert_eq!(retry.notice.notification_id, notice);
        let new = store.admit(id, retry.epoch, now).unwrap().unwrap();
        assert_eq!(new.subscription.device_token, "fixture-replacement-token");
        store
            .finish(
                retry,
                SendResult::InvalidSubscription,
                Some(new.revision),
                now,
            )
            .unwrap();
        assert!(
            store
                .registration(id, &owner, now)
                .unwrap()
                .capability
                .is_none()
        );
        assert!(matches!(
            publish(&mut store, &sender, NoticeKind::TurnCompleted, 10),
            Err(Error::Retired)
        ));
    }
}
