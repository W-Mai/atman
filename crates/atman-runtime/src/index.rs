use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub struct AnchorIndex {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl AnchorIndex {
    pub fn open_session(session_dir: &Path) -> Result<Self> {
        Self::open_with_schema(&session_dir.join("anchors.db"), SESSION_SCHEMA)
    }

    pub fn open_project(project_dir: &Path) -> Result<Self> {
        Self::open_with_schema(&project_dir.join("index.db"), PROJECT_SCHEMA)
    }

    fn open_with_schema(path: &Path, schema: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(schema)
            .with_context(|| format!("apply schema on {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Mutex::new(conn),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}

const SESSION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY,
    ts          TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    turn_id     TEXT,
    flow_run_id TEXT,
    payload     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS events_kind ON events(kind);
CREATE INDEX IF NOT EXISTS events_turn ON events(turn_id);
CREATE INDEX IF NOT EXISTS events_flow ON events(flow_run_id);

CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    text_content,
    content='events',
    content_rowid='seq',
    tokenize='porter unicode61'
);
"#;

const PROJECT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS anchors (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT NOT NULL,
    ref           TEXT NOT NULL,
    subject_kind  TEXT NOT NULL,
    subject_id    TEXT NOT NULL,
    session_id    TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS anchors_lookup  ON anchors(kind, ref);
CREATE INDEX IF NOT EXISTS anchors_subject ON anchors(subject_kind, subject_id);

CREATE TABLE IF NOT EXISTS confessions (
    id            TEXT PRIMARY KEY,
    trigger       TEXT NOT NULL,
    rule_violated TEXT NOT NULL,
    what_i_did    TEXT NOT NULL,
    why           TEXT NOT NULL,
    mitigation    TEXT NOT NULL,
    body          TEXT NOT NULL,
    created_at    TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS confessions_fts USING fts5(
    trigger, rule_violated, what_i_did, why, mitigation, body,
    content='confessions', content_rowid='rowid', tokenize='porter unicode61'
);

CREATE TABLE IF NOT EXISTS spec_entries (
    id      TEXT PRIMARY KEY,
    feature TEXT NOT NULL,
    phase   TEXT NOT NULL,
    content TEXT NOT NULL,
    ts      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS spec_entries_feature ON spec_entries(feature);
CREATE VIRTUAL TABLE IF NOT EXISTS spec_entries_fts USING fts5(
    content, content='spec_entries', content_rowid='rowid', tokenize='porter unicode61'
);

CREATE TABLE IF NOT EXISTS spec_deviations (
    id      TEXT PRIMARY KEY,
    feature TEXT NOT NULL,
    section TEXT NOT NULL,
    delta   TEXT NOT NULL,
    reason  TEXT NOT NULL,
    ts      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS spec_deviations_feature ON spec_deviations(feature);
CREATE VIRTUAL TABLE IF NOT EXISTS spec_deviations_fts USING fts5(
    delta, reason, content='spec_deviations', content_rowid='rowid',
    tokenize='porter unicode61'
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn tables_in(idx: &AnchorIndex) -> Vec<String> {
        let conn = idx.conn();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY name")
            .unwrap();
        stmt.query_map(params![], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    fn open_session_creates_events_and_events_fts() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_session(dir.path()).unwrap();
        let tables = tables_in(&idx);
        assert!(tables.iter().any(|t| t == "events"), "tables: {tables:?}");
        assert!(
            tables.iter().any(|t| t == "events_fts"),
            "tables: {tables:?}"
        );
    }

    #[test]
    fn open_project_creates_all_four_tables() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        let tables = tables_in(&idx);
        for expected in [
            "anchors",
            "confessions",
            "confessions_fts",
            "spec_entries",
            "spec_entries_fts",
            "spec_deviations",
            "spec_deviations_fts",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing {expected} in {tables:?}"
            );
        }
    }

    #[test]
    fn reopening_the_same_db_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let _one = AnchorIndex::open_project(dir.path()).unwrap();
        let two = AnchorIndex::open_project(dir.path()).unwrap();
        let tables = tables_in(&two);
        assert!(tables.iter().any(|t| t == "confessions"));
    }

    #[test]
    fn wal_journal_mode_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_session(dir.path()).unwrap();
        let conn = idx.conn();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
