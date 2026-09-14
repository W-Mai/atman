use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{collections::HashMap, io::Write};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::error::RuntimeError;
use crate::index::AnchorIndex;
use crate::memory::MemoryId;
#[cfg(test)]
use crate::memory::read_jsonl;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confession {
    pub id: MemoryId,
    pub trigger: String,
    pub rule_violated: String,
    pub what_i_did: String,
    pub why: String,
    pub mitigation: String,
    #[serde(default)]
    pub anchors: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfessionFields {
    pub trigger: String,
    pub rule_violated: String,
    pub what_i_did: String,
    pub why: String,
    pub mitigation: String,
}

impl From<&Confession> for ConfessionFields {
    fn from(value: &Confession) -> Self {
        Self {
            trigger: value.trigger.clone(),
            rule_violated: value.rule_violated.clone(),
            what_i_did: value.what_i_did.clone(),
            why: value.why.clone(),
            mitigation: value.mitigation.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfessionChange {
    Revised {
        id: MemoryId,
        base_revision: u64,
        fields: ConfessionFields,
        changed_at: chrono::DateTime<chrono::Utc>,
    },
    Organized {
        id: MemoryId,
        base_revision: u64,
        category: String,
        related_ids: Vec<MemoryId>,
        changed_at: chrono::DateTime<chrono::Utc>,
    },
    Archived {
        id: MemoryId,
        base_revision: u64,
        reason: String,
        changed_at: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ChangeLogEntry {
    Single(ConfessionChange),
    Batch { changes: Vec<ConfessionChange> },
}

impl ConfessionChange {
    fn id(&self) -> &MemoryId {
        match self {
            Self::Revised { id, .. } | Self::Organized { id, .. } | Self::Archived { id, .. } => id,
        }
    }

    fn base_revision(&self) -> u64 {
        match self {
            Self::Revised { base_revision, .. }
            | Self::Organized { base_revision, .. }
            | Self::Archived { base_revision, .. } => *base_revision,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfessionView {
    pub confession: Confession,
    pub revision: u64,
    pub category: Option<String>,
    pub related_ids: Vec<MemoryId>,
    pub archived: bool,
}

impl Confession {
    fn md_slug(&self) -> String {
        let date = self.created_at.format("%Y-%m-%d");
        let slug: String = self
            .trigger
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .take(48)
            .collect::<String>()
            .to_lowercase();
        let slug = if slug.is_empty() {
            "trigger".into()
        } else {
            slug
        };
        format!("{date}-{slug}-{}.md", &self.id.to_string()[..8])
    }

    fn render_md(&self) -> String {
        format!(
            "# {trigger}\n\n\
             - **id**: `{id}`\n\
             - **rule_violated**: {rule}\n\
             - **created_at**: {ts}\n\n\
             ## What I did\n\n{what}\n\n\
             ## Why\n\n{why}\n\n\
             ## Mitigation\n\n{mit}\n",
            trigger = self.trigger,
            id = self.id,
            rule = self.rule_violated,
            ts = self.created_at.to_rfc3339(),
            what = self.what_i_did,
            why = self.why,
            mit = self.mitigation,
        )
    }
}

pub struct ConfessionStore {
    dir: PathBuf,
    index_path: PathBuf,
    changes_path: PathBuf,
    anchor_index: Option<Arc<AnchorIndex>>,
    redactor: Option<Arc<crate::redact::Redactor>>,
}

impl ConfessionStore {
    pub fn at(scope_dir: impl AsRef<Path>) -> Self {
        let dir = scope_dir.as_ref().to_path_buf();
        let index_path = dir.join("confessions.jsonl");
        let changes_path = dir.join("changes.jsonl");
        Self {
            dir,
            index_path,
            changes_path,
            anchor_index: None,
            redactor: None,
        }
    }

    pub fn with_index(mut self, index: Arc<AnchorIndex>) -> Self {
        self.anchor_index = Some(index);
        self
    }

    pub fn with_redactor(mut self, redactor: Arc<crate::redact::Redactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    pub async fn append(&self, confession: Confession) -> Result<MemoryId, RuntimeError> {
        let confession = self.redact_if_needed(confession);
        let id = confession.id.clone();
        let dir = self.dir.clone();
        let index_path = self.index_path.clone();
        let for_write = confession.clone();
        let anchor_index = self.anchor_index.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&dir, || {
                let md_path = dir.join(for_write.md_slug());
                std::fs::write(&md_path, for_write.render_md()).map_err(store_error)?;
                append_line(&index_path, &for_write)?;
                if let Some(index) = &anchor_index
                    && let Err(error) = insert_confession(index, &for_write)
                {
                    crate::notify!(
                        warn,
                        "confession index insert failed (id={}): {error}",
                        for_write.id
                    );
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("confession writer: {e}")))??;
        Ok(id)
    }

    fn redact_if_needed(&self, mut c: Confession) -> Confession {
        let Some(r) = &self.redactor else {
            return c;
        };
        c.trigger = r.redact(&c.trigger).0;
        c.rule_violated = r.redact(&c.rule_violated).0;
        c.what_i_did = r.redact(&c.what_i_did).0;
        c.why = r.redact(&c.why).0;
        c.mitigation = r.redact(&c.mitigation).0;
        c
    }

    pub async fn find_by_trigger_fts(
        &self,
        query: &str,
    ) -> Result<Option<Vec<Confession>>, RuntimeError> {
        let Some(idx) = self.anchor_index.as_deref() else {
            return Ok(None);
        };
        let conn = idx.conn();
        let mut stmt = conn
            .prepare(
                "SELECT c.id, c.trigger, c.rule_violated, c.what_i_did, c.why, c.mitigation, c.created_at \
                 FROM confessions c \
                 JOIN confessions_fts f ON f.rowid = c.rowid \
                 WHERE f.confessions_fts MATCH ? \
                 ORDER BY c.rowid",
            )
            .map_err(|e| RuntimeError::ToolFailed(format!("fts prepare: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![query], |row| {
                let created_at: String = row.get(6)?;
                let created = chrono::DateTime::parse_from_rfc3339(&created_at)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                let id_str: String = row.get(0)?;
                let id = uuid::Uuid::parse_str(&id_str)
                    .map(MemoryId)
                    .unwrap_or_else(|_| MemoryId::now());
                Ok(Confession {
                    id,
                    trigger: row.get(1)?,
                    rule_violated: row.get(2)?,
                    what_i_did: row.get(3)?,
                    why: row.get(4)?,
                    mitigation: row.get(5)?,
                    anchors: Vec::new(),
                    created_at: created,
                })
            })
            .map_err(|e| RuntimeError::ToolFailed(format!("fts query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok(c) => out.push(c),
                Err(e) => return Err(RuntimeError::ToolFailed(format!("fts row: {e}"))),
            }
        }
        Ok(Some(out))
    }

    pub async fn list(&self) -> Result<Vec<Confession>, RuntimeError> {
        Ok(self
            .list_with_meta(false)
            .await?
            .into_iter()
            .map(|view| view.confession)
            .collect())
    }

    pub async fn list_with_meta(
        &self,
        include_archived: bool,
    ) -> Result<Vec<ConfessionView>, RuntimeError> {
        let dir = self.dir.clone();
        let index_path = self.index_path.clone();
        let changes_path = self.changes_path.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&dir, || {
                project_changes(
                    read_lines(&index_path)?,
                    read_change_lines(&changes_path)?,
                    include_archived,
                )
            })
        })
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("confession reader: {e}")))?
    }

    pub async fn history(&self, id: &MemoryId) -> Result<Vec<ConfessionChange>, RuntimeError> {
        let dir = self.dir.clone();
        let changes_path = self.changes_path.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&dir, || {
                Ok(read_change_lines(&changes_path)?
                    .into_iter()
                    .filter(|change| change.id() == &id)
                    .collect())
            })
        })
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("confession reader: {e}")))?
    }

    pub async fn revise(
        &self,
        id: MemoryId,
        base_revision: u64,
        mut fields: ConfessionFields,
    ) -> Result<ConfessionView, RuntimeError> {
        if let Some(redactor) = &self.redactor {
            fields.trigger = redactor.redact(&fields.trigger).0;
            fields.rule_violated = redactor.redact(&fields.rule_violated).0;
            fields.what_i_did = redactor.redact(&fields.what_i_did).0;
            fields.why = redactor.redact(&fields.why).0;
            fields.mitigation = redactor.redact(&fields.mitigation).0;
        }
        let change = ConfessionChange::Revised {
            id: id.clone(),
            base_revision,
            fields,
            changed_at: chrono::Utc::now(),
        };
        Ok(self.append_changes(vec![change]).await?.remove(0))
    }

    pub async fn organize(
        &self,
        changes: Vec<ConfessionChange>,
    ) -> Result<Vec<ConfessionView>, RuntimeError> {
        if changes
            .iter()
            .any(|change| !matches!(change, ConfessionChange::Organized { .. }))
        {
            return Err(RuntimeError::ToolFailed(
                "expected organization changes".into(),
            ));
        }
        self.append_changes(changes).await
    }

    async fn append_changes(
        &self,
        changes: Vec<ConfessionChange>,
    ) -> Result<Vec<ConfessionView>, RuntimeError> {
        let dir = self.dir.clone();
        let index_path = self.index_path.clone();
        let changes_path = self.changes_path.clone();
        let anchor_index = self.anchor_index.clone();
        tokio::task::spawn_blocking(move || {
            with_store_lock(&dir, || {
                let originals = read_lines::<Confession>(&index_path)?;
                let existing = read_change_lines(&changes_path)?;
                let current = project_changes(originals.clone(), existing.clone(), true)?;
                let by_id: HashMap<_, _> = current
                    .iter()
                    .map(|view| (view.confession.id.clone(), view.revision))
                    .collect();
                let mut seen = std::collections::HashSet::new();
                for change in &changes {
                    if !seen.insert(change.id().clone()) {
                        return Err(RuntimeError::ToolFailed(
                            "duplicate confession id in batch".into(),
                        ));
                    }
                    if by_id.get(change.id()) != Some(&change.base_revision()) {
                        return Err(RuntimeError::ToolFailed(
                            "confession revision conflict".into(),
                        ));
                    }
                    if let ConfessionChange::Organized {
                        category,
                        related_ids,
                        ..
                    } = change
                    {
                        if category.trim().is_empty() || category.chars().count() > 60 {
                            return Err(RuntimeError::ToolFailed(
                                "invalid confession category".into(),
                            ));
                        }
                        if related_ids
                            .iter()
                            .any(|related| !by_id.contains_key(related))
                        {
                            return Err(RuntimeError::ToolFailed(
                                "unknown related confession".into(),
                            ));
                        }
                    }
                }
                append_line(
                    &changes_path,
                    &ChangeLogEntry::Batch {
                        changes: changes.clone(),
                    },
                )?;
                let all = project_changes(
                    originals,
                    existing
                        .into_iter()
                        .chain(changes.iter().cloned())
                        .collect(),
                    true,
                )?;
                if let Some(index) = &anchor_index {
                    for view in all.iter().filter(|view| {
                        changes.iter().any(|change| {
                            change.id() == &view.confession.id
                                && !matches!(change, ConfessionChange::Organized { .. })
                        })
                    }) {
                        let result = if view.archived {
                            remove_confession_index(index, &view.confession.id)
                        } else {
                            insert_confession(index, &view.confession)
                        };
                        if let Err(error) = result {
                            crate::notify!(
                                warn,
                                "confession index update failed (id={}): {error}",
                                view.confession.id
                            );
                        }
                    }
                }
                Ok(all
                    .into_iter()
                    .filter(|view| seen.contains(&view.confession.id))
                    .collect())
            })
        })
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("confession writer: {e}")))?
    }

    pub async fn find_by_trigger(&self, needle: &str) -> Result<Vec<Confession>, RuntimeError> {
        if tokio::fs::try_exists(&self.changes_path)
            .await
            .unwrap_or(false)
        {
            let needle = needle.to_lowercase();
            return Ok(self
                .list()
                .await?
                .into_iter()
                .filter(|c| {
                    [
                        &c.trigger,
                        &c.rule_violated,
                        &c.what_i_did,
                        &c.why,
                        &c.mitigation,
                    ]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&needle))
                })
                .collect());
        }
        if let Ok(Some(hits)) = self.find_by_trigger_fts(needle).await
            && !hits.is_empty()
        {
            return Ok(hits);
        }
        let all = self.list().await?;
        Ok(all
            .into_iter()
            .filter(|c| c.trigger.contains(needle))
            .collect())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }
}

fn store_error(error: std::io::Error) -> RuntimeError {
    RuntimeError::ToolFailed(format!("confession storage: {error}"))
}

fn with_store_lock<T>(
    dir: &Path,
    operation: impl FnOnce() -> Result<T, RuntimeError>,
) -> Result<T, RuntimeError> {
    std::fs::create_dir_all(dir).map_err(store_error)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(".confessions.lock"))
        .map_err(store_error)?;
    fs2::FileExt::lock_exclusive(&lock).map_err(store_error)?;
    operation()
}

fn append_line(path: &Path, value: &impl Serialize) -> Result<(), RuntimeError> {
    append_lines(path, std::slice::from_ref(value))
}

fn append_lines(path: &Path, values: &[impl Serialize]) -> Result<(), RuntimeError> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(store_error)?;
    let mut output = Vec::new();
    for value in values {
        let mut line = serde_json::to_vec(value)
            .map_err(|e| RuntimeError::ToolFailed(format!("encode confession: {e}")))?;
        line.push(b'\n');
        output.extend(line);
    }
    file.write_all(&output).map_err(store_error)?;
    file.flush().map_err(store_error)
}

fn read_change_lines(path: &Path) -> Result<Vec<ConfessionChange>, RuntimeError> {
    Ok(flatten_changes(read_lines::<ChangeLogEntry>(path)?))
}

fn flatten_changes(entries: Vec<ChangeLogEntry>) -> Vec<ConfessionChange> {
    entries
        .into_iter()
        .flat_map(|entry| match entry {
            ChangeLogEntry::Single(change) => vec![change],
            ChangeLogEntry::Batch { changes } => changes,
        })
        .collect()
}

fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, RuntimeError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(store_error(error)),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| RuntimeError::ToolFailed(format!("decode confession: {error}")))
        })
        .collect()
}

fn project_changes(
    originals: Vec<Confession>,
    changes: Vec<ConfessionChange>,
    include_archived: bool,
) -> Result<Vec<ConfessionView>, RuntimeError> {
    let mut views = originals
        .into_iter()
        .map(|confession| ConfessionView {
            confession,
            revision: 0,
            category: None,
            related_ids: Vec::new(),
            archived: false,
        })
        .collect::<Vec<_>>();
    let positions = views
        .iter()
        .enumerate()
        .map(|(index, view)| (view.confession.id.clone(), index))
        .collect::<HashMap<_, _>>();
    for change in changes {
        let position = *positions.get(change.id()).ok_or_else(|| {
            RuntimeError::ToolFailed("confession change refers to missing id".into())
        })?;
        let view = &mut views[position];
        if view.revision != change.base_revision() {
            return Err(RuntimeError::ToolFailed(
                "confession change revision mismatch".into(),
            ));
        }
        match change {
            ConfessionChange::Revised { fields, .. } => {
                view.confession.trigger = fields.trigger;
                view.confession.rule_violated = fields.rule_violated;
                view.confession.what_i_did = fields.what_i_did;
                view.confession.why = fields.why;
                view.confession.mitigation = fields.mitigation;
            }
            ConfessionChange::Organized {
                category,
                related_ids,
                ..
            } => {
                view.category = Some(category);
                view.related_ids = related_ids;
            }
            ConfessionChange::Archived { .. } => view.archived = true,
        }
        view.revision += 1;
    }
    if !include_archived {
        views.retain(|view| !view.archived);
    }
    Ok(views)
}

fn remove_confession_index(index: &AnchorIndex, id: &MemoryId) -> rusqlite::Result<()> {
    let mut conn = index.conn();
    let tx = conn.transaction()?;
    let rowid = tx
        .query_row(
            "SELECT rowid FROM confessions WHERE id = ?",
            rusqlite::params![id.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(rowid) = rowid {
        tx.execute(
            "DELETE FROM confessions_fts WHERE rowid = ?",
            rusqlite::params![rowid],
        )?;
        tx.execute(
            "DELETE FROM confessions WHERE id = ?",
            rusqlite::params![id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM anchors WHERE subject_kind = 'confession' AND subject_id = ?",
            rusqlite::params![id.to_string()],
        )?;
    }
    tx.commit()
}

fn insert_confession(index: &AnchorIndex, c: &Confession) -> rusqlite::Result<()> {
    let mut conn = index.conn();
    let tx = conn.transaction()?;
    let old_rowid = tx
        .query_row(
            "SELECT rowid FROM confessions WHERE id = ?",
            rusqlite::params![c.id.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(rowid) = old_rowid {
        tx.execute(
            "DELETE FROM confessions_fts WHERE rowid = ?",
            rusqlite::params![rowid],
        )?;
    }
    let body = c.render_md();
    tx.execute(
        "INSERT OR REPLACE INTO confessions \
           (id, trigger, rule_violated, what_i_did, why, mitigation, body, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            c.id.to_string(),
            c.trigger,
            c.rule_violated,
            c.what_i_did,
            c.why,
            c.mitigation,
            body,
            c.created_at.to_rfc3339(),
        ],
    )?;
    let rowid: i64 = tx.last_insert_rowid();
    tx.execute(
        "INSERT OR REPLACE INTO confessions_fts \
           (rowid, trigger, rule_violated, what_i_did, why, mitigation, body) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            rowid,
            c.trigger,
            c.rule_violated,
            c.what_i_did,
            c.why,
            c.mitigation,
            body,
        ],
    )?;
    tx.execute(
        "DELETE FROM anchors WHERE subject_kind = 'confession' AND subject_id = ?",
        rusqlite::params![c.id.to_string()],
    )?;
    for anchor in &c.anchors {
        if let Some((kind, r)) = anchor.split_once(':') {
            tx.execute(
                "INSERT INTO anchors (kind, ref, subject_kind, subject_id, session_id, created_at) \
                 VALUES (?, ?, 'confession', ?, NULL, ?)",
                rusqlite::params![kind, r, c.id.to_string(), c.created_at.to_rfc3339()],
            )?;
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(trigger: &str, rule: &str) -> Confession {
        Confession {
            id: MemoryId::now(),
            trigger: trigger.into(),
            rule_violated: rule.into(),
            what_i_did: "wrote `as any`".into(),
            why: "was in a hurry".into(),
            mitigation: "run cargo check on every edit".into(),
            anchors: vec![],
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn append_then_list_returns_confession() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        let id = store
            .append(sample("you keep doing X", "no-as-any"))
            .await
            .unwrap();
        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
    }

    #[tokio::test]
    async fn find_by_trigger_filters() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        store
            .append(sample("comment discipline", "no-narrative-comments"))
            .await
            .unwrap();
        store
            .append(sample("type safety", "no-as-any"))
            .await
            .unwrap();
        let hits = store.find_by_trigger("comment").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule_violated, "no-narrative-comments");
    }

    #[tokio::test]
    async fn empty_returns_empty() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_with_index_populates_confessions_and_fts() {
        let dir = TempDir::new().unwrap();
        let index = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = ConfessionStore::at(dir.path()).with_index(index.clone());
        let mut c = sample("comment discipline yet again", "no-narrative-comments");
        c.anchors = vec!["flow_run:00000000-0000-0000-0000-000000000001".into()];
        store.append(c).await.unwrap();

        let conn = index.conn();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM confessions",
                rusqlite::params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let fts_hit: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM confessions_fts WHERE confessions_fts MATCH ?",
                rusqlite::params!["narrative"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_hit, 1, "fts should find `narrative` in rule_violated");

        let anchor_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM anchors WHERE kind='flow_run'",
                rusqlite::params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(anchor_count, 1);
    }

    #[tokio::test]
    async fn revision_preserves_history_and_changes_agent_search() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        let original = sample("old trigger", "old rule");
        let id = original.id.clone();
        store.append(original.clone()).await.unwrap();
        let mut fields = ConfessionFields::from(&original);
        fields.trigger = "new trigger".into();
        fields.mitigation = "new mitigation".into();
        let revised = store.revise(id.clone(), 0, fields).await.unwrap();
        assert_eq!(revised.revision, 1);
        assert_eq!(store.list().await.unwrap()[0].trigger, "new trigger");
        assert_eq!(store.find_by_trigger("old trigger").await.unwrap().len(), 0);
        assert_eq!(store.find_by_trigger("new trigger").await.unwrap().len(), 1);
        assert_eq!(store.history(&id).await.unwrap().len(), 1);
        let raw: Vec<Confession> = read_jsonl(&store.index_path).await.unwrap();
        assert_eq!(raw[0].trigger, "old trigger");
        assert!(
            store
                .revise(id, 0, ConfessionFields::from(&original))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn organization_batch_rejects_conflict_without_partial_write() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        let first = sample("first", "rule");
        let second = sample("second", "rule");
        store.append(first.clone()).await.unwrap();
        store.append(second.clone()).await.unwrap();
        let make = |id, base_revision| ConfessionChange::Organized {
            id,
            base_revision,
            category: "group".into(),
            related_ids: Vec::new(),
            changed_at: chrono::Utc::now(),
        };
        let result = store
            .organize(vec![make(first.id.clone(), 0), make(second.id.clone(), 1)])
            .await;
        assert!(result.is_err());
        assert!(store.history(&first.id).await.unwrap().is_empty());
        store
            .organize(vec![make(first.id.clone(), 0), make(second.id.clone(), 0)])
            .await
            .unwrap();
        let rows = store.list_with_meta(false).await.unwrap();
        assert!(
            rows.iter()
                .all(|view| view.category.as_deref() == Some("group"))
        );
        assert_eq!(
            read_lines::<ChangeLogEntry>(&store.changes_path)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn revision_replaces_fts_and_anchor_projection() {
        let dir = TempDir::new().unwrap();
        let index = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = ConfessionStore::at(dir.path().join("confessions")).with_index(index.clone());
        let mut original = sample("old trigger", "rule");
        original.anchors.push("turn:one".into());
        store.append(original.clone()).await.unwrap();
        let mut fields = ConfessionFields::from(&original);
        fields.trigger = "new trigger".into();
        store.revise(original.id.clone(), 0, fields).await.unwrap();
        let conn = index.conn();
        let fts_old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM confessions_fts WHERE confessions_fts MATCH 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let fts_new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM confessions_fts WHERE confessions_fts MATCH 'new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let anchors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM anchors WHERE subject_kind = 'confession' AND subject_id = ?",
                rusqlite::params![original.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((fts_old, fts_new, anchors), (0, 1, 1));
    }

    #[tokio::test]
    async fn archived_record_remains_in_history_but_leaves_search_index() {
        let dir = TempDir::new().unwrap();
        let index = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = ConfessionStore::at(dir.path().join("confessions")).with_index(index.clone());
        let original = sample("old trigger", "rule");
        store.append(original.clone()).await.unwrap();
        store
            .append_changes(vec![ConfessionChange::Archived {
                id: original.id.clone(),
                base_revision: 0,
                reason: "superseded".into(),
                changed_at: chrono::Utc::now(),
            }])
            .await
            .unwrap();
        assert!(store.list().await.unwrap().is_empty());
        assert_eq!(store.list_with_meta(true).await.unwrap().len(), 1);
        let count: i64 = index
            .conn()
            .query_row("SELECT COUNT(*) FROM confessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn find_by_trigger_fts_returns_none_without_index() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        assert!(store.find_by_trigger_fts("x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_by_trigger_fts_returns_matching_rows() {
        let dir = TempDir::new().unwrap();
        let index = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = ConfessionStore::at(dir.path()).with_index(index);
        store
            .append(sample("boot flow crash", "no-panic-in-boot"))
            .await
            .unwrap();
        store
            .append(sample("type safety again", "no-as-any"))
            .await
            .unwrap();
        let hits = store.find_by_trigger_fts("boot").await.unwrap().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule_violated, "no-panic-in-boot");
    }

    #[tokio::test]
    async fn append_writes_md_body_alongside_index() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        store
            .append(sample("comment discipline again", "no-narrative-comments"))
            .await
            .unwrap();
        let mut md_files = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut found_md = false;
        while let Some(entry) = md_files.next_entry().await.unwrap() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".md") {
                found_md = true;
                let body = tokio::fs::read_to_string(entry.path()).await.unwrap();
                assert!(body.starts_with("# comment discipline again"));
                assert!(body.contains("no-narrative-comments"));
            }
        }
        assert!(found_md, "expected a `.md` body file next to the index");
    }
}
