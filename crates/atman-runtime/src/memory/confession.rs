use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::RuntimeError;
use crate::index::AnchorIndex;
use crate::memory::{MemoryId, append_jsonl, read_jsonl};

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
    anchor_index: Option<Arc<AnchorIndex>>,
    redactor: Option<Arc<crate::redact::Redactor>>,
}

impl ConfessionStore {
    pub fn at(scope_dir: impl AsRef<Path>) -> Self {
        let dir = scope_dir.as_ref().to_path_buf();
        let index_path = dir.join("confessions.jsonl");
        Self {
            dir,
            index_path,
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
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| RuntimeError::ToolFailed(format!("mkdir {}: {e}", self.dir.display())))?;
        let md_path = self.dir.join(confession.md_slug());
        tokio::fs::write(&md_path, confession.render_md())
            .await
            .map_err(|e| RuntimeError::ToolFailed(format!("write {}: {e}", md_path.display())))?;
        append_jsonl(&self.index_path, &confession).await?;
        if let Some(idx) = &self.anchor_index
            && let Err(e) = insert_confession(idx, &confession)
        {
            eprintln!("[atman] confession index insert failed (id={id}): {e}");
        }
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
        read_jsonl(&self.index_path).await
    }

    pub async fn find_by_trigger(&self, needle: &str) -> Result<Vec<Confession>, RuntimeError> {
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

fn insert_confession(index: &AnchorIndex, c: &Confession) -> rusqlite::Result<()> {
    let conn = index.conn();
    let body = c.render_md();
    conn.execute(
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
    let rowid: i64 = conn.last_insert_rowid();
    conn.execute(
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
    for anchor in &c.anchors {
        if let Some((kind, r)) = anchor.split_once(':') {
            conn.execute(
                "INSERT INTO anchors (kind, ref, subject_kind, subject_id, session_id, created_at) \
                 VALUES (?, ?, 'confession', ?, NULL, ?)",
                rusqlite::params![kind, r, c.id.to_string(), c.created_at.to_rfc3339()],
            )?;
        }
    }
    Ok(())
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
