use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActor {
    User,
    Agent,
    Parent,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPauseReason {
    UserRequested,
    ParentRequested,
    Completed,
    NeedsInput,
    Blocked,
    Cancelled,
    RunFailed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalPause {
    pub actor: GoalActor,
    pub reason: GoalPauseReason,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRecord {
    pub id: String,
    pub revision: u64,
    pub objective: String,
    pub owner: String,
    pub parent_goal_id: Option<String>,
    pub status: GoalStatus,
    pub pause: Option<GoalPause>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl GoalRecord {
    pub fn new(objective: String, owner: String, parent_goal_id: Option<String>) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            revision: 1,
            objective,
            owner,
            parent_goal_id,
            status: GoalStatus::Active,
            pause: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn pause(&mut self, actor: GoalActor, reason: GoalPauseReason, summary: String) {
        self.status = GoalStatus::Paused;
        self.pause = Some(GoalPause {
            actor,
            reason,
            summary,
        });
    }
}

pub struct GoalStore {
    path: PathBuf,
    legacy_path: Option<PathBuf>,
    notify: Option<tokio::sync::watch::Sender<Option<String>>>,
}

impl GoalStore {
    pub fn at(session_dir: impl AsRef<Path>) -> Self {
        Self {
            path: session_dir.as_ref().join("goal.json"),
            legacy_path: Some(session_dir.as_ref().join("goal.txt")),
            notify: None,
        }
    }

    pub fn for_child(session_dir: &Path, goal_id: &str) -> std::io::Result<Self> {
        // Only generated UUIDs can become storage path components.
        let id = uuid::Uuid::parse_str(goal_id).map_err(std::io::Error::other)?;
        Ok(Self {
            path: session_dir.join("goals").join(format!("{id}.json")),
            legacy_path: None,
            notify: None,
        })
    }

    pub fn with_notify(mut self, tx: tokio::sync::watch::Sender<Option<String>>) -> Self {
        self.notify = Some(tx);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> std::io::Result<String> {
        Ok(self.load()?.map(|g| g.objective).unwrap_or_default())
    }

    pub fn set(&self, text: &str) -> std::io::Result<()> {
        self.update(None, |record| {
            let text = text.trim_end().to_owned();
            if let Some(goal) = record {
                goal.objective = text;
            } else {
                *record = Some(GoalRecord::new(text, String::new(), None));
            }
            Ok(())
        })
        .map(|_| ())
    }

    pub fn clear(&self) -> std::io::Result<()> {
        self.update(None, |record| {
            *record = None;
            Ok(())
        })
        .map(|_| ())
    }

    pub fn load(&self) -> std::io::Result<Option<GoalRecord>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => return serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let Some(path) = &self.legacy_path else {
            return Ok(None);
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text.trim_end().to_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if text.is_empty() {
            return Ok(None);
        }
        let mut goal = GoalRecord::new(text, String::new(), None);
        goal.id = format!(
            "legacy-{}",
            blake3::hash(path.as_os_str().as_encoded_bytes())
        );
        goal.created_at = chrono::DateTime::UNIX_EPOCH;
        goal.updated_at = goal.created_at;
        goal.pause(
            GoalActor::Runtime,
            GoalPauseReason::Interrupted,
            "Saved goal; resume explicitly to continue.".into(),
        );
        Ok(Some(goal))
    }

    /// Atomically update a goal, rejecting stale request identities before mutation.
    pub fn update(
        &self,
        expected: Option<(&str, u64)>,
        change: impl FnOnce(&mut Option<GoalRecord>) -> std::io::Result<()>,
    ) -> std::io::Result<Option<GoalRecord>> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let mut record = self.load()?;
        if let Some((id, revision)) = expected
            && !record
                .as_ref()
                .is_some_and(|g| g.id == id && g.revision == revision)
        {
            return Err(std::io::Error::other(
                "stale goal: read the current goal before updating",
            ));
        }
        let previous = record.clone();
        change(&mut record)?;
        if let Some(goal) = &mut record
            && previous.as_ref() != Some(goal)
        {
            if let Some(old) = previous.as_ref().filter(|old| old.id == goal.id) {
                goal.revision = old
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| std::io::Error::other("goal revision exhausted"))?;
            }
            goal.updated_at = chrono::Utc::now();
        }
        let temp = parent.join(format!(".goal-{}.tmp", uuid::Uuid::now_v7()));
        let bytes = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
        let result = (|| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result?;
        if let Some(tx) = &self.notify {
            tx.send_replace(record.as_ref().map(|g| g.objective.clone()));
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_never_set() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        assert_eq!(g.get().unwrap(), "");
    }

    #[test]
    fn set_then_get_roundtrips_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.set("ship an atman agent\n\n").unwrap();
        assert_eq!(g.get().unwrap(), "ship an atman agent");
    }

    #[test]
    fn set_overwrites_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.set("v1").unwrap();
        g.set("v2").unwrap();
        assert_eq!(g.get().unwrap(), "v2");
    }

    #[test]
    fn clear_writes_tombstone_and_get_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.set("temporary").unwrap();
        g.clear().unwrap();
        assert_eq!(g.get().unwrap(), "");
        assert_eq!(std::fs::read_to_string(g.path()).unwrap(), "null");
    }

    #[test]
    fn clear_on_missing_file_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.clear().unwrap();
    }

    #[test]
    fn set_creates_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested/deep");
        let g = GoalStore::at(&nested);
        g.set("hi").unwrap();
        assert_eq!(g.get().unwrap(), "hi");
    }

    #[test]
    fn legacy_goals_never_start_or_reappear_after_clear() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("goal.txt"), "old task").unwrap();
        let store = GoalStore::at(tmp.path());
        let legacy = store.load().unwrap().unwrap();
        assert_eq!(legacy.status, GoalStatus::Paused);
        assert_eq!(store.load().unwrap().unwrap(), legacy);
        store.clear().unwrap();
        assert!(GoalStore::at(tmp.path()).load().unwrap().is_none());
    }

    #[test]
    fn revisions_reject_stale_writers_and_text_edits_preserve_pause() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GoalStore::at(tmp.path());
        store.set("work").unwrap();
        let old = store.load().unwrap().unwrap();
        store
            .update(Some((&old.id, old.revision)), |record| {
                record.as_mut().unwrap().pause(
                    GoalActor::User,
                    GoalPauseReason::UserRequested,
                    "stop".into(),
                );
                Ok(())
            })
            .unwrap();
        assert!(
            store
                .update(Some((&old.id, old.revision)), |_| Ok(()))
                .is_err()
        );
        store.set("revised work").unwrap();
        assert_eq!(store.load().unwrap().unwrap().status, GoalStatus::Paused);
    }
}
