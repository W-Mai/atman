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

    pub fn find_events_around(&self, seq: u64, window: usize) -> Result<Vec<EventRow>> {
        let low = seq.saturating_sub(window as u64) as i64;
        let high = seq.saturating_add(window as u64) as i64;
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, turn_id, flow_run_id, payload FROM events \
             WHERE seq BETWEEN ? AND ? ORDER BY seq",
        )?;
        let rows = stmt.query_map(rusqlite::params![low, high], event_row_from)?;
        collect(rows)
    }

    pub fn find_events_by_anchor(&self, kind: AnchorKind, id: &str) -> Result<Vec<EventRow>> {
        let sql = format!(
            "SELECT seq, ts, kind, turn_id, flow_run_id, payload FROM events \
             WHERE {} = ? ORDER BY seq",
            kind.events_column()
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![id], event_row_from)?;
        collect(rows)
    }

    pub fn fts_search_events(&self, query: &str, limit: usize) -> Result<Vec<EventRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload \
             FROM events e JOIN events_fts f ON f.rowid = e.seq \
             WHERE f.events_fts MATCH ? LIMIT ?",
        )?;
        let rows = stmt.query_map(rusqlite::params![query, limit as i64], event_row_from)?;
        collect(rows)
    }

    pub fn find_by_anchor(&self, kind: AnchorKind, id: &str) -> Result<Vec<(String, String)>> {
        let sql =
            "SELECT subject_kind, subject_id FROM anchors WHERE kind = ? AND ref = ? ORDER BY id";
        let conn = self.conn();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params![kind.anchor_tag(), id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    TurnId,
    FlowRunId,
}

impl AnchorKind {
    fn events_column(self) -> &'static str {
        match self {
            AnchorKind::TurnId => "turn_id",
            AnchorKind::FlowRunId => "flow_run_id",
        }
    }

    fn anchor_tag(self) -> &'static str {
        match self {
            AnchorKind::TurnId => "turn",
            AnchorKind::FlowRunId => "flow_run",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub turn_id: Option<String>,
    pub flow_run_id: Option<String>,
    pub payload: String,
}

fn event_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        seq: row.get::<_, i64>(0)? as u64,
        ts: row.get(1)?,
        kind: row.get(2)?,
        turn_id: row.get(3)?,
        flow_run_id: row.get(4)?,
        payload: row.get(5)?,
    })
}

fn collect<T>(
    iter: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for r in iter {
        out.push(r.map_err(|e| anyhow::anyhow!(e))?);
    }
    Ok(out)
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

    fn seed_events(idx: &AnchorIndex) {
        let conn = idx.conn();
        for (seq, kind, turn, flow, text) in [
            (1i64, "flow_start", None, Some("run-a"), None),
            (2, "user_msg", Some("turn-a"), None, Some("hello world")),
            (
                3,
                "assistant_msg",
                Some("turn-a"),
                Some("run-a"),
                Some("sqlite full-text search"),
            ),
            (4, "flow_end", None, Some("run-a"), None),
            (5, "flow_start", None, Some("run-b"), None),
        ] {
            conn.execute(
                "INSERT INTO events (seq, ts, kind, turn_id, flow_run_id, payload) VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    seq,
                    "2026-07-05T00:00:00Z",
                    kind,
                    turn,
                    flow,
                    format!("{{\"seq\":{seq}}}"),
                ],
            )
            .unwrap();
            if let Some(text_content) = text {
                conn.execute(
                    "INSERT INTO events_fts (rowid, text_content) VALUES (?, ?)",
                    rusqlite::params![seq, text_content],
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn find_events_around_returns_inclusive_window() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_session(dir.path()).unwrap();
        seed_events(&idx);
        let rows = idx.find_events_around(3, 1).unwrap();
        let seqs: Vec<u64> = rows.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4]);
    }

    #[test]
    fn find_events_by_anchor_filters_by_flow_run() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_session(dir.path()).unwrap();
        seed_events(&idx);
        let rows = idx
            .find_events_by_anchor(AnchorKind::FlowRunId, "run-a")
            .unwrap();
        let seqs: Vec<u64> = rows.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![1, 3, 4]);
    }

    #[test]
    fn fts_search_events_matches_text_content() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_session(dir.path()).unwrap();
        seed_events(&idx);
        let rows = idx.fts_search_events("sqlite", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 3);
    }

    #[test]
    fn find_by_anchor_returns_subject_kinds_from_project_db() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        {
            let conn = idx.conn();
            conn.execute(
                "INSERT INTO anchors (kind, ref, subject_kind, subject_id, created_at) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params!["flow_run", "run-xyz", "confession", "cid-1", "2026-07-05T00:00:00Z"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO anchors (kind, ref, subject_kind, subject_id, created_at) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params!["flow_run", "run-xyz", "spec_entry", "sid-1", "2026-07-05T00:00:00Z"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO anchors (kind, ref, subject_kind, subject_id, created_at) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params!["turn_id", "turn-99", "confession", "cid-2", "2026-07-05T00:00:00Z"],
            )
            .unwrap();
        }
        let hits = idx
            .find_by_anchor(AnchorKind::FlowRunId, "run-xyz")
            .unwrap();
        assert_eq!(
            hits,
            vec![
                ("confession".to_string(), "cid-1".to_string()),
                ("spec_entry".to_string(), "sid-1".to_string()),
            ]
        );
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
