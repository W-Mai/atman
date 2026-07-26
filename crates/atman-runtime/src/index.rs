use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub struct AnchorIndex {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl AnchorIndex {
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

    pub fn insert_project_event(
        &self,
        session_id: &str,
        event: &crate::event::Event,
        payload_json: &str,
    ) -> rusqlite::Result<i64> {
        let ts = chrono::Utc::now().to_rfc3339();
        let kind = crate::event_writer::event_kind(event);
        let (turn_id, flow_run_id) = crate::event_writer::extract_anchors(event);
        let text_content = crate::event_writer::extract_text_content(event).unwrap_or_default();
        let seq = 0_i64;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR REPLACE INTO events \
             (session_id, seq, ts, kind, turn_id, flow_run_id, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                session_id,
                seq,
                ts,
                kind,
                turn_id,
                flow_run_id,
                payload_json
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT OR REPLACE INTO events_fts (rowid, text_content) VALUES (?, ?)",
            rusqlite::params![id, text_content],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn fts_search_project_events(
        &self,
        query: &str,
        session_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ProjectEventRow>> {
        let conn = self.conn();
        // CJK text doesn't tokenize well under unicode61 — use LIKE for CJK queries.
        if query.chars().any(is_cjk_char) {
            return self.search_events_like(query, session_filter, limit, &conn);
        }
        let (sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match session_filter {
            Some(sid) => (
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload \
                 FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.events_fts MATCH ?1 AND e.session_id = ?2 \
                 ORDER BY e.id DESC LIMIT ?3"
                    .into(),
                vec![
                    Box::new(query.to_string()),
                    Box::new(sid.to_string()),
                    Box::new(limit as i64),
                ],
            ),
            None => (
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload \
                 FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.events_fts MATCH ?1 \
                 ORDER BY e.id DESC LIMIT ?2"
                    .into(),
                vec![Box::new(query.to_string()), Box::new(limit as i64)],
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), project_event_row_from)?;
        collect(rows)
    }

    fn search_events_like(
        &self,
        query: &str,
        session_filter: Option<&str>,
        limit: usize,
        conn: &std::sync::MutexGuard<'_, rusqlite::Connection>,
    ) -> Result<Vec<ProjectEventRow>> {
        let pattern = format!("%{query}%");
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match session_filter {
            Some(sid) => (
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload \
                 FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.text_content LIKE ?1 AND e.session_id = ?2 \
                 ORDER BY e.id DESC LIMIT ?3",
                vec![
                    Box::new(pattern),
                    Box::new(sid.to_string()),
                    Box::new(limit as i64),
                ],
            ),
            None => (
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload \
                 FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.text_content LIKE ?1 \
                 ORDER BY e.id DESC LIMIT ?2",
                vec![Box::new(pattern), Box::new(limit as i64)],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), project_event_row_from)?;
        collect(rows)
    }

    pub fn count_events(&self, session_id: &str, kinds: Option<&[&str]>) -> Result<u64> {
        let conn = self.conn();
        let (sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match kinds {
            Some(ks) if !ks.is_empty() => {
                let placeholders = ks.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT COUNT(*) FROM events WHERE session_id = ? AND kind IN ({placeholders})"
                );
                let mut params: Vec<Box<dyn rusqlite::ToSql>> =
                    vec![Box::new(session_id.to_string())];
                for k in ks {
                    params.push(Box::new(k.to_string()));
                }
                (sql, params)
            }
            _ => (
                "SELECT COUNT(*) FROM events WHERE session_id = ?".into(),
                vec![Box::new(session_id.to_string())],
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn read_events_paginated(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
        kinds: Option<&[&str]>,
    ) -> Result<Vec<ProjectEventRow>> {
        let conn = self.conn();
        let (sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match kinds {
            Some(ks) if !ks.is_empty() => {
                let placeholders = ks.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload \
                     FROM events WHERE session_id = ? AND kind IN ({placeholders}) \
                     ORDER BY seq LIMIT ? OFFSET ?"
                );
                let mut params: Vec<Box<dyn rusqlite::ToSql>> =
                    vec![Box::new(session_id.to_string())];
                for k in ks {
                    params.push(Box::new(k.to_string()));
                }
                params.push(Box::new(limit as i64));
                params.push(Box::new(offset as i64));
                (sql, params)
            }
            _ => (
                "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload \
                 FROM events WHERE session_id = ? ORDER BY seq LIMIT ? OFFSET ?"
                    .into(),
                vec![
                    Box::new(session_id.to_string()),
                    Box::new(limit as i64),
                    Box::new(offset as i64),
                ],
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), project_event_row_from)?;
        collect(rows)
    }

    pub fn count_search_hits(&self, query: &str, session_filter: Option<&str>) -> Result<u64> {
        let conn = self.conn();
        if query.chars().any(is_cjk_char) {
            return self.count_search_hits_like(query, session_filter, &conn);
        }
        let (sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match session_filter {
            Some(sid) => (
                "SELECT COUNT(*) FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.events_fts MATCH ?1 AND e.session_id = ?2"
                    .into(),
                vec![Box::new(query.to_string()), Box::new(sid.to_string())],
            ),
            None => (
                "SELECT COUNT(*) FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.events_fts MATCH ?1"
                    .into(),
                vec![Box::new(query.to_string())],
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
        Ok(count as u64)
    }

    fn count_search_hits_like(
        &self,
        query: &str,
        session_filter: Option<&str>,
        conn: &std::sync::MutexGuard<'_, rusqlite::Connection>,
    ) -> Result<u64> {
        let pattern = format!("%{query}%");
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match session_filter {
            Some(sid) => (
                "SELECT COUNT(*) FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.text_content LIKE ?1 AND e.session_id = ?2",
                vec![Box::new(pattern), Box::new(sid.to_string())],
            ),
            None => (
                "SELECT COUNT(*) FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.text_content LIKE ?1",
                vec![Box::new(pattern)],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn delete_events_for_session(&self, session_id: &str) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM events_fts WHERE rowid IN (SELECT id FROM events WHERE session_id = ?)",
            rusqlite::params![session_id],
        )?;
        let n = tx.execute(
            "DELETE FROM events WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.commit()?;
        Ok(n)
    }

    pub fn insert_project_event_raw(&self, row: ProjectEventInsert<'_>) -> rusqlite::Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR REPLACE INTO events \
             (session_id, seq, ts, kind, turn_id, flow_run_id, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                row.session_id,
                row.seq,
                row.ts,
                row.kind,
                row.turn_id,
                row.flow_run_id,
                row.payload_json,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT OR REPLACE INTO events_fts (rowid, text_content) VALUES (?, ?)",
            rusqlite::params![id, row.text_content],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn find_project_events_around(
        &self,
        session_id: &str,
        seq: u64,
        window: usize,
    ) -> Result<Vec<ProjectEventRow>> {
        let low = seq.saturating_sub(window as u64) as i64;
        let high = seq.saturating_add(window as u64) as i64;
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload FROM events \
             WHERE session_id = ? AND seq BETWEEN ? AND ? ORDER BY seq",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![session_id, low, high],
            project_event_row_from,
        )?;
        collect(rows)
    }

    pub fn find_project_events_by_anchor(
        &self,
        session_id: &str,
        kind: AnchorKind,
        id: &str,
    ) -> Result<Vec<ProjectEventRow>> {
        let sql = format!(
            "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload FROM events \
             WHERE session_id = ? AND {} = ? ORDER BY seq",
            kind.events_column()
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![session_id, id], project_event_row_from)?;
        collect(rows)
    }

    pub fn rebuild_events_from_sessions(
        &self,
        sessions_root: &Path,
        fingerprint: &str,
    ) -> Result<RebuildStats> {
        let mut stats = RebuildStats::default();
        let read = match std::fs::read_dir(sessions_root) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
            Err(e) => return Err(e).context(format!("read_dir {}", sessions_root.display())),
        };
        for entry in read.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(meta) = crate::session_meta::SessionMeta::load(&dir) else {
                continue;
            };
            if meta.project_fingerprint.as_deref() != Some(fingerprint) {
                continue;
            }
            let sid = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let jsonl = dir.join("events.jsonl");
            let envelopes = match crate::event_log::reader::read_event_envelopes(&jsonl) {
                Ok(envelopes) => envelopes,
                Err(_) => continue,
            };
            for envelope in &envelopes {
                let value = serde_json::to_value(envelope)?;
                let seq = envelope.seq as i64;
                let ts = envelope.ts.to_rfc3339();
                let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let turn_id = value.get("turn_id").and_then(|v| v.as_str());
                let flow_run_id = value
                    .get("run_id")
                    .or_else(|| value.get("flow_run_id"))
                    .and_then(|v| v.as_str());
                let text_content = value
                    .get("message")
                    .and_then(|m| m.get("parts"))
                    .and_then(|p| p.as_array())
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                let payload_json = serde_json::to_string(&value).unwrap_or_default();
                self.insert_project_event_raw(ProjectEventInsert {
                    session_id: &sid,
                    seq,
                    ts: &ts,
                    kind,
                    turn_id,
                    flow_run_id,
                    text_content: &text_content,
                    payload_json: &payload_json,
                })?;
                stats.rebuilt += 1;
            }
        }
        Ok(stats)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebuildStats {
    pub rebuilt: usize,
    pub skipped: usize,
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

pub struct ProjectEventInsert<'a> {
    pub session_id: &'a str,
    pub seq: i64,
    pub ts: &'a str,
    pub kind: &'a str,
    pub turn_id: Option<&'a str>,
    pub flow_run_id: Option<&'a str>,
    pub text_content: &'a str,
    pub payload_json: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEventRow {
    pub session_id: String,
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub turn_id: Option<String>,
    pub flow_run_id: Option<String>,
    pub payload: String,
}

fn project_event_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectEventRow> {
    Ok(ProjectEventRow {
        session_id: row.get(0)?,
        seq: row.get::<_, i64>(1)? as u64,
        ts: row.get(2)?,
        kind: row.get(3)?,
        turn_id: row.get(4)?,
        flow_run_id: row.get(5)?,
        payload: row.get(6)?,
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

fn is_cjk_char(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF   |  // CJK Unified Ideographs
        0x3400..=0x4DBF   |  // CJK Extension A
        0x20000..=0x2A6DF |  // CJK Extension B
        0x3040..=0x309F   |  // Hiragana
        0x30A0..=0x30FF   |  // Katakana
        0xAC00..=0xD7AF      // Hangul Syllables
    )
}

const PROJECT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT    NOT NULL,
    seq         INTEGER NOT NULL,
    ts          TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    turn_id     TEXT,
    flow_run_id TEXT,
    payload     TEXT    NOT NULL,
    UNIQUE (session_id, seq)
);
CREATE INDEX IF NOT EXISTS events_session ON events(session_id);
CREATE INDEX IF NOT EXISTS events_kind    ON events(kind);
CREATE INDEX IF NOT EXISTS events_turn    ON events(turn_id);
CREATE INDEX IF NOT EXISTS events_flow    ON events(flow_run_id);

CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    text_content,
    tokenize='porter unicode61'
);

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
    tokenize='porter unicode61'
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
    content, tokenize='porter unicode61'
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
    delta, reason, tokenize='porter unicode61'
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
    fn fts_search_finds_cjk_substring() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(
            &idx,
            "sess-a",
            1,
            "user_msg",
            None,
            None,
            "这是一个浮动面板的设计方案",
        );
        // 2-char CJK substring — would fail with FTS5 unicode61
        let hits = idx
            .fts_search_project_events("浮动", Some("sess-a"), 10)
            .unwrap();
        assert_eq!(hits.len(), 1, "2-char CJK substring should match");
        // 4-char CJK phrase
        let hits = idx
            .fts_search_project_events("浮动面板", Some("sess-a"), 10)
            .unwrap();
        assert_eq!(hits.len(), 1, "4-char CJK phrase should match");
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
            "events",
            "events_fts",
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

    fn seed_project_event(
        idx: &AnchorIndex,
        sid: &str,
        seq: i64,
        kind: &str,
        turn: Option<&str>,
        flow: Option<&str>,
        text: &str,
    ) {
        let payload = format!("{{\"seq\":{seq},\"session_id\":\"{sid}\"}}");
        idx.insert_project_event_raw(ProjectEventInsert {
            session_id: sid,
            seq,
            ts: "2026-07-05T00:00:00Z",
            kind,
            turn_id: turn,
            flow_run_id: flow,
            text_content: text,
            payload_json: &payload,
        })
        .unwrap();
    }

    #[test]
    fn fts_search_project_events_filters_by_session_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(
            &idx,
            "sess-a",
            1,
            "user_msg",
            Some("t1"),
            None,
            "hello sqlite fts",
        );
        seed_project_event(
            &idx,
            "sess-b",
            1,
            "user_msg",
            Some("t2"),
            None,
            "sqlite from other session",
        );
        seed_project_event(
            &idx,
            "sess-a",
            2,
            "assistant_msg",
            Some("t1"),
            None,
            "no match here",
        );

        let scoped = idx
            .fts_search_project_events("sqlite", Some("sess-a"), 10)
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session_id, "sess-a");
        assert_eq!(scoped[0].seq, 1);

        let all = idx.fts_search_project_events("sqlite", None, 10).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn insert_project_event_upsert_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "sess-a", 42, "user_msg", None, None, "uniquetoken");
        seed_project_event(&idx, "sess-a", 42, "user_msg", None, None, "uniquetoken");
        let hits = idx
            .fts_search_project_events("uniquetoken", None, 10)
            .unwrap();
        assert_eq!(hits.len(), 1, "same (session_id, seq) must upsert");
    }

    #[test]
    fn find_project_events_around_scopes_to_session() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "sess-a", 1, "flow_start", None, Some("r"), "");
        seed_project_event(&idx, "sess-a", 2, "user_msg", Some("t"), None, "hello");
        seed_project_event(
            &idx,
            "sess-a",
            3,
            "assistant_msg",
            Some("t"),
            Some("r"),
            "world",
        );
        seed_project_event(&idx, "sess-a", 4, "flow_end", None, Some("r"), "");
        seed_project_event(&idx, "sess-b", 3, "user_msg", None, None, "unrelated");

        let rows = idx.find_project_events_around("sess-a", 3, 1).unwrap();
        let seqs: Vec<u64> = rows.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4]);
        assert!(rows.iter().all(|r| r.session_id == "sess-a"));
    }

    #[test]
    fn find_project_events_by_anchor_scopes_to_session() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "sess-a", 1, "flow_start", None, Some("run-x"), "");
        seed_project_event(&idx, "sess-a", 2, "flow_end", None, Some("run-x"), "");
        seed_project_event(&idx, "sess-b", 1, "flow_start", None, Some("run-x"), "");

        let rows = idx
            .find_project_events_by_anchor("sess-a", AnchorKind::FlowRunId, "run-x")
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.session_id == "sess-a"));
    }

    #[test]
    fn rebuild_events_from_sessions_filters_by_fingerprint() {
        let sessions = tempfile::tempdir().unwrap();
        let index_dir = tempfile::tempdir().unwrap();

        let sess_ours = sessions.path().join("sess-a");
        let sess_theirs = sessions.path().join("sess-b");
        std::fs::create_dir_all(&sess_ours).unwrap();
        std::fs::create_dir_all(&sess_theirs).unwrap();

        crate::session_meta::SessionMeta {
            project_fingerprint: Some("ours".into()),
            ..Default::default()
        }
        .save(&sess_ours)
        .unwrap();
        crate::session_meta::SessionMeta {
            project_fingerprint: Some("theirs".into()),
            ..Default::default()
        }
        .save(&sess_theirs)
        .unwrap();

        let tid1 = uuid::Uuid::now_v7();
        let rid1 = uuid::Uuid::now_v7();
        let events_ours = format!(
            r#"{{"type":"user_msg","seq":1,"turn_id":"{tid1}","ts":"2026-07-05T00:00:00Z","message":{{"role":"user","parts":[{{"type":"text","text":"needle in ours"}}],"turn_id":"{tid1}"}}}}
{{"type":"flow_end","seq":2,"run_id":"{rid1}","flow_name":"demo","status":{{"kind":"ok"}},"ts":"2026-07-05T00:00:01Z"}}"#
        );
        std::fs::write(sess_ours.join("events.jsonl"), events_ours).unwrap();

        let tid2 = uuid::Uuid::now_v7();
        let events_theirs = format!(
            r#"{{"type":"user_msg","seq":1,"turn_id":"{tid2}","ts":"2026-07-05T00:00:00Z","message":{{"role":"user","parts":[{{"type":"text","text":"needle in theirs"}}],"turn_id":"{tid2}"}}}}"#
        );
        std::fs::write(sess_theirs.join("events.jsonl"), events_theirs).unwrap();

        let idx = AnchorIndex::open_project(index_dir.path()).unwrap();
        let stats = idx
            .rebuild_events_from_sessions(sessions.path(), "ours")
            .unwrap();
        assert_eq!(stats.rebuilt, 2);

        let hits = idx.fts_search_project_events("needle", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess-a");
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
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        let conn = idx.conn();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn count_events_returns_zero_for_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        assert_eq!(idx.count_events("no-such-session", None).unwrap(), 0);
    }

    #[test]
    fn count_events_counts_all_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "hello");
        seed_project_event(&idx, "s1", 2, "assistant_msg", None, None, "hi");
        seed_project_event(&idx, "s1", 3, "tool_result_msg", None, None, "ok");
        seed_project_event(&idx, "s2", 1, "user_msg", None, None, "other");
        assert_eq!(idx.count_events("s1", None).unwrap(), 3);
        assert_eq!(idx.count_events("s2", None).unwrap(), 1);
    }

    #[test]
    fn count_events_filters_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "a");
        seed_project_event(&idx, "s1", 2, "assistant_msg", None, None, "b");
        seed_project_event(&idx, "s1", 3, "tool_result_msg", None, None, "c");
        seed_project_event(&idx, "s1", 4, "user_msg", None, None, "d");
        assert_eq!(idx.count_events("s1", Some(&["user_msg"])).unwrap(), 2);
        assert_eq!(
            idx.count_events("s1", Some(&["user_msg", "assistant_msg"]))
                .unwrap(),
            3
        );
        assert_eq!(idx.count_events("s1", Some(&["system_msg"])).unwrap(), 0);
    }

    #[test]
    fn read_events_paginated_returns_ordered_by_seq() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 3, "user_msg", None, None, "third");
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "first");
        seed_project_event(&idx, "s1", 2, "user_msg", None, None, "second");
        let rows = idx.read_events_paginated("s1", 0, 10, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[1].seq, 2);
        assert_eq!(rows[2].seq, 3);
    }

    #[test]
    fn read_events_paginated_respects_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        for i in 1..=5 {
            seed_project_event(&idx, "s1", i, "user_msg", None, None, &format!("msg {i}"));
        }
        let rows = idx.read_events_paginated("s1", 1, 2, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 2);
        assert_eq!(rows[1].seq, 3);
    }

    #[test]
    fn read_events_paginated_filters_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "u1");
        seed_project_event(&idx, "s1", 2, "assistant_msg", None, None, "a1");
        seed_project_event(&idx, "s1", 3, "user_msg", None, None, "u2");
        let rows = idx
            .read_events_paginated("s1", 0, 10, Some(&["user_msg"]))
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[1].seq, 3);
    }

    #[test]
    fn read_events_paginated_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        let rows = idx.read_events_paginated("no-such", 0, 10, None).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn count_search_hits_fts() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "hello world");
        seed_project_event(&idx, "s1", 2, "user_msg", None, None, "hello again");
        seed_project_event(&idx, "s1", 3, "user_msg", None, None, "goodbye");
        seed_project_event(&idx, "s2", 1, "user_msg", None, None, "hello other");
        assert_eq!(idx.count_search_hits("hello", None).unwrap(), 3);
        assert_eq!(idx.count_search_hits("hello", Some("s1")).unwrap(), 2);
        assert_eq!(idx.count_search_hits("goodbye", None).unwrap(), 1);
        assert_eq!(idx.count_search_hits("nomatch", None).unwrap(), 0);
    }

    #[test]
    fn count_search_hits_cjk() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "浮动面板设计");
        seed_project_event(&idx, "s1", 2, "user_msg", None, None, "另一个消息");
        assert_eq!(idx.count_search_hits("浮动", None).unwrap(), 1);
        assert_eq!(idx.count_search_hits("浮动", Some("s1")).unwrap(), 1);
        assert_eq!(idx.count_search_hits("不存在", None).unwrap(), 0);
    }
}
