use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::flow_meta::{FlowMeta, FlowMetaSource};

pub struct FlowRegistry {
    path: PathBuf,
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRevision {
    pub id: i64,
    pub flow_name: String,
    pub version: String,
    pub content: String,
    pub content_hash: String,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub author: Option<String>,
    pub source_tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    Inserted(FlowRevision),
    UnchangedFromLatest(FlowRevision),
}

impl FlowRegistry {
    pub fn open(project_dir: &Path) -> Result<Self> {
        let path = project_dir.join(".atman").join("flow-registry.db");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("apply schema on {}", path.display()))?;
        Ok(Self {
            path,
            conn: Mutex::new(conn),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(
        &self,
        flow_name: &str,
        content: &str,
        meta: &FlowMeta,
    ) -> Result<SnapshotOutcome> {
        let content_hash = FlowMeta::short_hash(content);
        if let Some(latest) = self.latest(flow_name)?
            && latest.content_hash == content_hash
        {
            return Ok(SnapshotOutcome::UnchangedFromLatest(latest));
        }
        let ts = chrono::Utc::now();
        let source_tag = match meta.source {
            FlowMetaSource::Sidecar => "sidecar",
            FlowMetaSource::HashFallback => "hash",
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO flow_revisions \
             (flow_name, version, content, content_hash, ts, author, source_tag) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                flow_name,
                meta.version,
                content,
                content_hash,
                ts.to_rfc3339(),
                meta.author,
                source_tag,
            ],
        )?;
        let id = conn.last_insert_rowid();
        Ok(SnapshotOutcome::Inserted(FlowRevision {
            id,
            flow_name: flow_name.to_string(),
            version: meta.version.clone(),
            content: content.to_string(),
            content_hash,
            ts,
            author: meta.author.clone(),
            source_tag: source_tag.to_string(),
        }))
    }

    pub fn list_versions(&self, flow_name: &str) -> Result<Vec<FlowRevision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, flow_name, version, content, content_hash, ts, author, source_tag \
             FROM flow_revisions WHERE flow_name = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([flow_name], row_to_revision)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn latest(&self, flow_name: &str) -> Result<Option<FlowRevision>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, flow_name, version, content, content_hash, ts, author, source_tag \
             FROM flow_revisions WHERE flow_name = ?1 ORDER BY id DESC LIMIT 1",
            [flow_name],
            row_to_revision,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn find_by_version(
        &self,
        flow_name: &str,
        version_or_hash: &str,
    ) -> Result<Option<FlowRevision>> {
        let conn = self.conn.lock().unwrap();
        let exact = conn
            .query_row(
                "SELECT id, flow_name, version, content, content_hash, ts, author, source_tag \
                 FROM flow_revisions WHERE flow_name = ?1 AND version = ?2 \
                 ORDER BY id DESC LIMIT 1",
                params![flow_name, version_or_hash],
                row_to_revision,
            )
            .optional()?;
        if let Some(r) = exact {
            return Ok(Some(r));
        }
        let stripped = version_or_hash
            .strip_prefix("hash:")
            .unwrap_or(version_or_hash);
        if stripped.is_empty() {
            return Ok(None);
        }
        let prefix_pattern = format!("{stripped}%");
        conn.query_row(
            "SELECT id, flow_name, version, content, content_hash, ts, author, source_tag \
             FROM flow_revisions WHERE flow_name = ?1 AND content_hash LIKE ?2 \
             ORDER BY id DESC LIMIT 1",
            params![flow_name, prefix_pattern],
            row_to_revision,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn count(&self, flow_name: &str) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM flow_revisions WHERE flow_name = ?1",
            [flow_name],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    pub fn flow_names(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT DISTINCT flow_name FROM flow_revisions ORDER BY flow_name ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn row_to_revision(row: &rusqlite::Row<'_>) -> rusqlite::Result<FlowRevision> {
    let ts_str: String = row.get(5)?;
    let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    Ok(FlowRevision {
        id: row.get(0)?,
        flow_name: row.get(1)?,
        version: row.get(2)?,
        content: row.get(3)?,
        content_hash: row.get(4)?,
        ts,
        author: row.get(6)?,
        source_tag: row.get(7)?,
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS flow_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    flow_name TEXT NOT NULL,
    version TEXT NOT NULL,
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    ts TEXT NOT NULL,
    author TEXT,
    source_tag TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS flow_revisions_by_name_id
    ON flow_revisions (flow_name, id DESC);
CREATE INDEX IF NOT EXISTS flow_revisions_by_hash
    ON flow_revisions (flow_name, content_hash);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_meta::{FlowMeta, FlowMetaSource};

    fn mk_meta(version: &str, source: FlowMetaSource) -> FlowMeta {
        FlowMeta {
            version: version.to_string(),
            description: None,
            last_modified: None,
            author: Some("w-mai".into()),
            tags: Vec::new(),
            source,
        }
    }

    #[test]
    fn snapshot_inserts_and_returns_row() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        let out = reg
            .snapshot(
                "greet",
                "flow greet() { return 1 }",
                &mk_meta("0.1.0", FlowMetaSource::Sidecar),
            )
            .unwrap();
        let SnapshotOutcome::Inserted(rev) = out else {
            panic!("expected inserted, got {out:?}");
        };
        assert_eq!(rev.flow_name, "greet");
        assert_eq!(rev.version, "0.1.0");
        assert_eq!(rev.source_tag, "sidecar");
        assert_eq!(reg.count("greet").unwrap(), 1);
    }

    #[test]
    fn snapshot_skips_when_latest_is_identical() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        let src = "flow greet() { return 1 }";
        reg.snapshot("greet", src, &mk_meta("0.1.0", FlowMetaSource::Sidecar))
            .unwrap();
        let again = reg
            .snapshot("greet", src, &mk_meta("0.1.0", FlowMetaSource::Sidecar))
            .unwrap();
        assert!(matches!(again, SnapshotOutcome::UnchangedFromLatest(_)));
        assert_eq!(reg.count("greet").unwrap(), 1);
    }

    #[test]
    fn snapshot_appends_when_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        reg.snapshot(
            "greet",
            "flow greet() { return 1 }",
            &mk_meta("0.1.0", FlowMetaSource::Sidecar),
        )
        .unwrap();
        reg.snapshot(
            "greet",
            "flow greet() { return 2 }",
            &mk_meta("0.2.0", FlowMetaSource::Sidecar),
        )
        .unwrap();
        assert_eq!(reg.count("greet").unwrap(), 2);
        let versions = reg.list_versions("greet").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, "0.2.0");
        assert_eq!(versions[1].version, "0.1.0");
    }

    #[test]
    fn find_by_version_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        reg.snapshot(
            "greet",
            "flow greet() { return 1 }",
            &mk_meta("0.1.0", FlowMetaSource::Sidecar),
        )
        .unwrap();
        let hit = reg.find_by_version("greet", "0.1.0").unwrap().unwrap();
        assert_eq!(hit.version, "0.1.0");
        assert!(reg.find_by_version("greet", "9.9.9").unwrap().is_none());
    }

    #[test]
    fn find_by_version_short_hash_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        let src = "flow greet() { return 1 }";
        let hash = FlowMeta::short_hash(src);
        reg.snapshot(
            "greet",
            src,
            &mk_meta(&format!("hash:{hash}"), FlowMetaSource::HashFallback),
        )
        .unwrap();
        let short_prefix: String = hash.chars().take(6).collect();
        let hit = reg
            .find_by_version("greet", &short_prefix)
            .unwrap()
            .expect("prefix should match");
        assert_eq!(hit.content_hash, hash);
        let hit2 = reg
            .find_by_version("greet", &format!("hash:{short_prefix}"))
            .unwrap()
            .expect("hash: prefix should also match");
        assert_eq!(hit2.content_hash, hash);
    }

    #[test]
    fn flow_names_returns_distinct_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        reg.snapshot(
            "b_flow",
            "flow b_flow() { return 1 }",
            &mk_meta("1", FlowMetaSource::Sidecar),
        )
        .unwrap();
        reg.snapshot(
            "a_flow",
            "flow a_flow() { return 1 }",
            &mk_meta("1", FlowMetaSource::Sidecar),
        )
        .unwrap();
        reg.snapshot(
            "a_flow",
            "flow a_flow() { return 2 }",
            &mk_meta("2", FlowMetaSource::Sidecar),
        )
        .unwrap();
        let names = reg.flow_names().unwrap();
        assert_eq!(names, vec!["a_flow", "b_flow"]);
    }

    #[test]
    fn latest_returns_most_recent_revision() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        reg.snapshot("greet", "a", &mk_meta("0.1", FlowMetaSource::Sidecar))
            .unwrap();
        reg.snapshot("greet", "b", &mk_meta("0.2", FlowMetaSource::Sidecar))
            .unwrap();
        let latest = reg.latest("greet").unwrap().unwrap();
        assert_eq!(latest.version, "0.2");
        assert!(reg.latest("unknown").unwrap().is_none());
    }

    #[test]
    fn source_tag_reflects_meta_source() {
        let dir = tempfile::tempdir().unwrap();
        let reg = FlowRegistry::open(dir.path()).unwrap();
        reg.snapshot(
            "a",
            "x",
            &mk_meta("hash:aaaa", FlowMetaSource::HashFallback),
        )
        .unwrap();
        reg.snapshot("b", "y", &mk_meta("1.0", FlowMetaSource::Sidecar))
            .unwrap();
        assert_eq!(reg.latest("a").unwrap().unwrap().source_tag, "hash");
        assert_eq!(reg.latest("b").unwrap().unwrap().source_tag, "sidecar");
    }
}
