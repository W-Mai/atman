use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

pub struct AnchorIndex {
    path: PathBuf,
    conn: Mutex<Connection>,
}

/// Selects raw events or message-bearing events before counting and pagination.
#[derive(Clone, Copy)]
pub enum EventFilter<'a> {
    /// Every event, including events without messages.
    All,
    /// An empty list matches no events.
    Kinds(&'a [&'a str]),
    /// Empty roles select every message role.
    Messages(&'a [&'a str]),
}

impl EventFilter<'_> {
    fn predicate(self, params: &mut Vec<rusqlite::types::Value>) -> String {
        match self {
            Self::All => "1".into(),
            Self::Kinds(kinds) => sql_membership("kind", kinds.iter().copied(), params),
            Self::Messages(roles) => {
                let kinds = [
                    ("user", "user_msg"),
                    ("assistant", "assistant_msg"),
                    ("tool", "tool_result_msg"),
                    ("system", "system_msg"),
                ];
                let ordinary = sql_membership(
                    "kind",
                    kinds.iter().filter_map(|(role, kind)| {
                        (roles.is_empty() || roles.contains(role)).then_some(*kind)
                    }),
                    params,
                );
                let role = if roles.is_empty() {
                    String::new()
                } else {
                    format!(
                        " AND {}",
                        sql_membership(
                            "json_extract(payload, '$.context_message.role')",
                            roles.iter().copied(),
                            params,
                        )
                    )
                };
                format!(
                    "({ordinary} OR CASE WHEN kind = 'user_inject' THEN \
                     json_extract(payload, '$.injection.state') = 'injected' \
                     AND json_type(payload, '$.context_message') = 'object'{role} ELSE 0 END)"
                )
            }
        }
    }
}

fn sql_membership<'a>(
    column: &str,
    values: impl Iterator<Item = &'a str>,
    params: &mut Vec<rusqlite::types::Value>,
) -> String {
    let placeholders = values
        .map(|value| {
            params.push(value.to_string().into());
            "?"
        })
        .collect::<Vec<_>>()
        .join(",");
    if placeholders.is_empty() {
        "0".into()
    } else {
        format!("{column} IN ({placeholders})")
    }
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
    ) -> Result<Vec<ProjectEventSearchHit>> {
        let conn = self.conn();
        if let Some(pattern) = parse_regex_query(query) {
            return self.search_events_regex(&pattern, session_filter, limit, &conn);
        }
        self.search_events_like(query, session_filter, limit, &conn)
    }

    fn search_events_like(
        &self,
        query: &str,
        session_filter: Option<&str>,
        limit: usize,
        conn: &std::sync::MutexGuard<'_, rusqlite::Connection>,
    ) -> Result<Vec<ProjectEventSearchHit>> {
        let pattern = format!("%{query}%");
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match session_filter {
            Some(sid) => (
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload, f.text_content \
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
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload, f.text_content \
                 FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE f.text_content LIKE ?1 \
                 ORDER BY e.id DESC LIMIT ?2",
                vec![Box::new(pattern), Box::new(limit as i64)],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), project_event_search_hit_from)?;
        collect(rows)
    }

    pub fn count_events(&self, session_id: &str, filter: EventFilter<'_>) -> Result<u64> {
        let conn = self.conn();
        let mut params = vec![session_id.to_string().into()];
        let predicate = filter.predicate(&mut params);
        let sql = format!("SELECT COUNT(*) FROM events WHERE session_id = ? AND {predicate}");
        let mut stmt = conn.prepare(&sql)?;
        let count: i64 = stmt.query_row(rusqlite::params_from_iter(params), |row| row.get(0))?;
        Ok(count as u64)
    }

    fn search_events_regex(
        &self,
        pattern: &str,
        session_filter: Option<&str>,
        limit: usize,
        conn: &std::sync::MutexGuard<'_, rusqlite::Connection>,
    ) -> Result<Vec<ProjectEventSearchHit>> {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match session_filter {
            Some(sid) => (
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload, f.text_content \
                 FROM events e JOIN events_fts f ON f.rowid = e.id \
                 WHERE e.session_id = ?1 ORDER BY e.id DESC LIMIT 2000",
                vec![Box::new(sid.to_string())],
            ),
            None => (
                "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload, f.text_content \
                 FROM events e JOIN events_fts f ON f.rowid = e.id \
                 ORDER BY e.id DESC LIMIT 2000",
                vec![],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((
                project_event_search_hit_from(row)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (event, text) = row?;
            if re.is_match(&text) {
                hits.push(event);
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    pub fn read_events_paginated(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
        filter: EventFilter<'_>,
    ) -> Result<Vec<ProjectEventRow>> {
        let conn = self.conn();
        let mut params = vec![session_id.to_string().into()];
        let predicate = filter.predicate(&mut params);
        let sql = format!(
            "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload \
             FROM events WHERE session_id = ? AND {predicate} ORDER BY seq LIMIT ? OFFSET ?"
        );
        params.push((limit as i64).into());
        params.push((offset as i64).into());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), project_event_row_from)?;
        collect(rows)
    }

    pub fn read_events_before_descending(
        &self,
        session_id: &str,
        before_seq: Option<u64>,
        limit: usize,
        filter: EventFilter<'_>,
    ) -> Result<Vec<ProjectEventRow>> {
        let conn = self.conn();
        let mut params = vec![session_id.to_string().into()];
        let predicate = filter.predicate(&mut params);
        let sql = format!(
            "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload \
             FROM events WHERE session_id = ? AND {predicate} AND seq < ? \
             ORDER BY seq DESC LIMIT ?"
        );
        params.push(
            before_seq
                .map_or(i64::MAX, |seq| i64::try_from(seq).unwrap_or(i64::MAX))
                .into(),
        );
        params.push((limit as i64).into());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), project_event_row_from)?;
        collect(rows)
    }

    pub fn read_events_from_seq(
        &self,
        session_id: &str,
        start_seq: u64,
    ) -> Result<Vec<ProjectEventRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload \
             FROM events WHERE session_id = ? AND seq >= ? ORDER BY seq",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![session_id, i64::try_from(start_seq).unwrap_or(i64::MAX),],
            project_event_row_from,
        )?;
        collect(rows)
    }

    pub fn has_contiguous_event_range(
        &self,
        session_id: &str,
        start_seq: u64,
        end_seq: u64,
    ) -> Result<bool> {
        if start_seq > end_seq {
            return Ok(false);
        }
        let conn = self.conn();
        let count = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ? AND seq BETWEEN ? AND ?",
            rusqlite::params![
                session_id,
                i64::try_from(start_seq).unwrap_or(i64::MAX),
                i64::try_from(end_seq).unwrap_or(i64::MAX),
            ],
            |row| row.get::<_, u64>(0),
        )?;
        Ok(count == end_seq.saturating_sub(start_seq).saturating_add(1))
    }

    pub fn count_search_hits(&self, query: &str, session_filter: Option<&str>) -> Result<u64> {
        let conn = self.conn();
        if let Some(pattern) = parse_regex_query(query) {
            return Ok(self
                .search_events_regex(&pattern, session_filter, 10000, &conn)?
                .len() as u64);
        }
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
        tx.execute(
            "DELETE FROM timeline_turns WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM timeline_runs WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM timeline_event_owners WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM timeline_materializations WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM event_index_coverage WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.commit()?;
        Ok(n)
    }

    pub fn insert_project_event_raw(&self, row: ProjectEventInsert<'_>) -> rusqlite::Result<i64> {
        self.insert_project_event_row(row, None)
    }

    pub fn insert_project_event_at_boundary(
        &self,
        row: ProjectEventInsert<'_>,
        boundary: EventLogBoundary<'_>,
    ) -> rusqlite::Result<i64> {
        self.insert_project_event_row(row, Some(boundary))
    }

    fn insert_project_event_row(
        &self,
        row: ProjectEventInsert<'_>,
        boundary: Option<EventLogBoundary<'_>>,
    ) -> rusqlite::Result<i64> {
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
        if let (Some(turn_id), Some(flow_run_id)) = (row.turn_id, row.flow_run_id) {
            tx.execute(
                "INSERT INTO timeline_runs (session_id, flow_run_id, turn_id) VALUES (?, ?, ?) \
                 ON CONFLICT(session_id, flow_run_id) DO UPDATE SET turn_id = excluded.turn_id",
                rusqlite::params![row.session_id, flow_run_id, turn_id],
            )?;
        }
        let effective_turn = match row.turn_id {
            Some(turn_id) => Some(turn_id.to_owned()),
            None => match row.flow_run_id {
                Some(flow_run_id) => tx
                    .query_row(
                        "SELECT turn_id FROM timeline_runs \
                         WHERE session_id = ? AND flow_run_id = ?",
                        rusqlite::params![row.session_id, flow_run_id],
                        |query| query.get::<_, String>(0),
                    )
                    .optional()?,
                None => None,
            },
        };
        if let Some(turn_id) = effective_turn {
            tx.execute(
                "INSERT INTO timeline_turns (session_id, turn_id, start_seq, latest_seq) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(session_id, turn_id) DO UPDATE SET \
                 start_seq = MIN(start_seq, excluded.start_seq), \
                 latest_seq = MAX(latest_seq, excluded.latest_seq)",
                rusqlite::params![row.session_id, &turn_id, row.seq, row.seq],
            )?;
            tx.execute(
                "INSERT INTO timeline_event_owners (session_id, seq, turn_id) VALUES (?, ?, ?) \
                 ON CONFLICT(session_id, seq) DO UPDATE SET turn_id = excluded.turn_id",
                rusqlite::params![row.session_id, row.seq, turn_id],
            )?;
        }
        if let Some(boundary) = boundary {
            tx.execute(
                "INSERT INTO event_index_coverage \
                 (session_id, seq, line_start, line_end, log_offset, line_digest) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(session_id) DO UPDATE SET \
                 seq = excluded.seq, line_start = excluded.line_start, \
                 line_end = excluded.line_end, log_offset = excluded.log_offset, \
                 line_digest = excluded.line_digest \
                 WHERE excluded.seq >= event_index_coverage.seq",
                rusqlite::params![
                    row.session_id,
                    row.seq,
                    boundary.line_start,
                    boundary.line_end,
                    boundary.log_offset,
                    boundary.line_digest,
                ],
            )?;
        }
        tx.execute(
            "UPDATE timeline_materializations SET source_seq = MAX(source_seq, ?) \
             WHERE session_id = ?",
            rusqlite::params![row.seq, row.session_id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn materialize_timeline_session(&self, session_id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let source_seq = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?",
            rusqlite::params![session_id],
            |row| row.get::<_, i64>(0),
        )?;
        let materialized_seq = tx
            .query_row(
                "SELECT source_seq FROM timeline_materializations WHERE session_id = ?",
                rusqlite::params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if materialized_seq == Some(source_seq) {
            return Ok(());
        }

        tx.execute(
            "DELETE FROM timeline_turns WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM timeline_runs WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM timeline_event_owners WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "INSERT INTO timeline_runs (session_id, flow_run_id, turn_id) \
             SELECT session_id, flow_run_id, turn_id FROM events \
             WHERE session_id = ? AND flow_run_id IS NOT NULL AND turn_id IS NOT NULL \
             ORDER BY seq ON CONFLICT(session_id, flow_run_id) DO UPDATE SET \
             turn_id = excluded.turn_id",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "INSERT INTO timeline_event_owners (session_id, seq, turn_id) \
             SELECT e.session_id, e.seq, COALESCE(e.turn_id, r.turn_id) FROM events e \
             LEFT JOIN timeline_runs r ON r.session_id = e.session_id \
             AND r.flow_run_id = e.flow_run_id WHERE e.session_id = ? \
             AND COALESCE(e.turn_id, r.turn_id) IS NOT NULL",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "INSERT INTO timeline_turns (session_id, turn_id, start_seq, latest_seq) \
             SELECT session_id, turn_id, MIN(seq), MAX(seq) FROM timeline_event_owners \
             WHERE session_id = ? GROUP BY session_id, turn_id",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "INSERT INTO timeline_materializations (session_id, source_seq) VALUES (?, ?) \
             ON CONFLICT(session_id) DO UPDATE SET source_seq = excluded.source_seq",
            rusqlite::params![session_id, source_seq],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn backfill_session_events(
        &self,
        session_id: &str,
        events: &[crate::event::EventEnvelope],
    ) -> Result<usize> {
        let existing = {
            let conn = self.conn();
            let mut stmt = conn.prepare("SELECT seq FROM events WHERE session_id = ?")?;
            let rows = stmt.query_map(rusqlite::params![session_id], |row| {
                Ok(row.get::<_, i64>(0)? as u64)
            })?;
            let mut existing = std::collections::HashSet::new();
            for row in rows {
                existing.insert(row?);
            }
            existing
        };
        let missing = events
            .iter()
            .filter(|event| !existing.contains(&event.seq))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut inserted = 0;
        for envelope in missing {
            let payload = serde_json::to_string(envelope)?;
            let (turn_id, flow_run_id) = crate::event_writer::extract_anchors(&envelope.event);
            let text_content =
                crate::event_writer::extract_text_content(&envelope.event).unwrap_or_default();
            let changed = tx.execute(
                "INSERT OR IGNORE INTO events \
                 (session_id, seq, ts, kind, turn_id, flow_run_id, payload) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    session_id,
                    i64::try_from(envelope.seq).unwrap_or(i64::MAX),
                    envelope.ts.to_rfc3339(),
                    crate::event_writer::event_kind(&envelope.event),
                    turn_id,
                    flow_run_id,
                    payload,
                ],
            )?;
            if changed == 0 {
                continue;
            }
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT OR REPLACE INTO events_fts (rowid, text_content) VALUES (?, ?)",
                rusqlite::params![id, text_content],
            )?;
            inserted += 1;
        }
        tx.execute(
            "DELETE FROM timeline_materializations WHERE session_id = ?",
            rusqlite::params![session_id],
        )?;
        tx.commit()?;
        drop(conn);
        self.materialize_timeline_session(session_id)?;
        Ok(inserted)
    }

    pub fn read_turns_before(
        &self,
        session_id: &str,
        before_start_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<ProjectTurnRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT turn_id, start_seq, latest_seq FROM timeline_turns \
             WHERE session_id = ? AND start_seq < ? \
             ORDER BY start_seq DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                session_id,
                before_start_seq.map_or(i64::MAX, |seq| { i64::try_from(seq).unwrap_or(i64::MAX) }),
                limit as i64,
            ],
            |row| {
                Ok(ProjectTurnRow {
                    turn_id: row.get(0)?,
                    start_seq: row.get::<_, i64>(1)? as u64,
                    latest_seq: row.get::<_, i64>(2)? as u64,
                })
            },
        )?;
        collect(rows)
    }

    pub fn read_turns_after(
        &self,
        session_id: &str,
        after_start_seq: u64,
        limit: usize,
    ) -> Result<Vec<ProjectTurnRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT turn_id, start_seq, latest_seq FROM timeline_turns \
             WHERE session_id = ? AND start_seq > ? \
             ORDER BY start_seq ASC LIMIT ?",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                session_id,
                i64::try_from(after_start_seq).unwrap_or(i64::MAX),
                limit as i64,
            ],
            |row| {
                Ok(ProjectTurnRow {
                    turn_id: row.get(0)?,
                    start_seq: row.get::<_, i64>(1)? as u64,
                    latest_seq: row.get::<_, i64>(2)? as u64,
                })
            },
        )?;
        collect(rows)
    }

    pub fn read_event_at_seq(&self, session_id: &str, seq: u64) -> Result<Option<ProjectEventRow>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT session_id, seq, ts, kind, turn_id, flow_run_id, payload \
             FROM events WHERE session_id = ? AND seq = ?",
            rusqlite::params![session_id, i64::try_from(seq).unwrap_or(i64::MAX)],
            project_event_row_from,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn estimate_turn_event_bytes(
        &self,
        session_id: &str,
        turn: &ProjectTurnRow,
        through_seq: u64,
    ) -> Result<u64> {
        let conn = self.conn();
        let bytes = conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(e.payload)), 0) FROM events e \
             LEFT JOIN timeline_event_owners o \
             ON o.session_id = e.session_id AND o.seq = e.seq \
             WHERE e.session_id = ? AND e.seq BETWEEN ? AND ? \
             AND (o.turn_id = ? OR o.turn_id IS NULL)",
            rusqlite::params![
                session_id,
                i64::try_from(turn.start_seq).unwrap_or(i64::MAX),
                i64::try_from(through_seq.max(turn.latest_seq)).unwrap_or(i64::MAX),
                &turn.turn_id,
            ],
            |row| row.get::<_, u64>(0),
        )?;
        Ok(bytes)
    }

    pub fn count_turns_before(
        &self,
        session_id: &str,
        before_start_seq: Option<u64>,
    ) -> Result<u64> {
        let conn = self.conn();
        let count = conn.query_row(
            "SELECT COUNT(*) FROM timeline_turns WHERE session_id = ? AND start_seq < ?",
            rusqlite::params![
                session_id,
                before_start_seq.map_or(i64::MAX, |seq| { i64::try_from(seq).unwrap_or(i64::MAX) }),
            ],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count as u64)
    }

    pub fn read_events_for_turns(
        &self,
        session_id: &str,
        turns: &[ProjectTurnRow],
        include_unowned_through: Option<u64>,
    ) -> Result<Vec<ProjectEventRow>> {
        let Some(first_seq) = turns.iter().map(|turn| turn.start_seq).min() else {
            return Ok(Vec::new());
        };
        let latest_seq = turns
            .iter()
            .map(|turn| turn.latest_seq)
            .max()
            .unwrap_or(first_seq)
            .max(include_unowned_through.unwrap_or(first_seq));
        let mut params = vec![
            rusqlite::types::Value::from(session_id.to_owned()),
            i64::try_from(first_seq).unwrap_or(i64::MAX).into(),
            i64::try_from(latest_seq).unwrap_or(i64::MAX).into(),
        ];
        let owners = sql_membership(
            "o.turn_id",
            turns.iter().map(|turn| turn.turn_id.as_str()),
            &mut params,
        );
        let sql = format!(
            "SELECT e.session_id, e.seq, e.ts, e.kind, e.turn_id, e.flow_run_id, e.payload \
             FROM events e LEFT JOIN timeline_event_owners o \
             ON o.session_id = e.session_id AND o.seq = e.seq \
             WHERE e.session_id = ? AND e.seq BETWEEN ? AND ? \
             AND ({owners} OR o.turn_id IS NULL) ORDER BY e.seq"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), project_event_row_from)?;
        collect(rows)
    }

    pub fn turn_start_for_event(&self, session_id: &str, seq: u64) -> Result<Option<u64>> {
        let conn = self.conn();
        let start_seq = conn
            .query_row(
                "SELECT t.start_seq FROM timeline_event_owners o \
                 JOIN timeline_turns t ON t.session_id = o.session_id AND t.turn_id = o.turn_id \
                 WHERE o.session_id = ? AND o.seq = ?",
                rusqlite::params![session_id, i64::try_from(seq).unwrap_or(i64::MAX),],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(start_seq.map(|value| value as u64))
    }

    pub fn validated_event_coverage(
        &self,
        session_id: &str,
        events_path: &Path,
    ) -> Result<Option<EventIndexCoverage>> {
        let Some(mut coverage) = self.validated_event_tail(session_id, events_path)? else {
            return Ok(None);
        };
        coverage.start_seq = self.contiguous_event_suffix_start(session_id, coverage.seq)?;
        Ok(Some(coverage))
    }

    /// Validates the indexed log boundary without walking every indexed sequence.
    ///
    /// Timeline pagination only needs a trustworthy tail watermark and the oldest
    /// indexed sequence. Full contiguous coverage remains available through
    /// [`Self::validated_event_coverage`] for repair paths.
    pub fn validated_timeline_coverage(
        &self,
        session_id: &str,
        events_path: &Path,
    ) -> Result<Option<EventIndexCoverage>> {
        let Some(mut coverage) = self.validated_event_tail(session_id, events_path)? else {
            return Ok(None);
        };
        coverage.start_seq = {
            let conn = self.conn();
            conn.query_row(
                "SELECT MIN(seq) FROM events WHERE session_id = ?",
                rusqlite::params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .map_or(coverage.seq, |seq| seq as u64)
        };
        Ok(Some(coverage))
    }

    fn validated_event_tail(
        &self,
        session_id: &str,
        events_path: &Path,
    ) -> Result<Option<EventIndexCoverage>> {
        let (coverage, max_seq, indexed_payload) = {
            let conn = self.conn();
            let coverage = conn
                .query_row(
                    "SELECT seq, line_start, line_end, log_offset, line_digest \
                     FROM event_index_coverage WHERE session_id = ?",
                    rusqlite::params![session_id],
                    |row| {
                        Ok(EventIndexCoverage {
                            start_seq: 0,
                            seq: row.get::<_, i64>(0)? as u64,
                            line_start: row.get::<_, i64>(1)? as u64,
                            line_end: row.get::<_, i64>(2)? as u64,
                            log_offset: row.get::<_, i64>(3)? as u64,
                            line_digest: row.get(4)?,
                        })
                    },
                )
                .optional()?;
            let Some(coverage) = coverage else {
                return Ok(None);
            };
            let max_seq = conn.query_row(
                "SELECT MAX(seq) FROM events WHERE session_id = ?",
                rusqlite::params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )?;
            let indexed_payload = conn
                .query_row(
                    "SELECT payload FROM events WHERE session_id = ? AND seq = ?",
                    rusqlite::params![session_id, coverage.seq as i64],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            (coverage, max_seq, indexed_payload)
        };
        let Some(indexed_payload) = indexed_payload else {
            return Ok(None);
        };
        if max_seq.map(|seq| seq as u64) != Some(coverage.seq)
            || blake3::hash(indexed_payload.as_bytes()).to_hex().as_str() != coverage.line_digest
        {
            return Ok(None);
        }
        let metadata = std::fs::metadata(events_path)
            .with_context(|| format!("inspect {}", events_path.display()))?;
        if metadata.len() != coverage.log_offset
            || coverage.line_start > coverage.line_end
            || coverage.line_end > coverage.log_offset
        {
            return Ok(None);
        }
        let mut file = std::fs::File::open(events_path)
            .with_context(|| format!("open {}", events_path.display()))?;
        file.seek(SeekFrom::Start(coverage.line_start))?;
        let mut remaining = coverage.line_end.saturating_sub(coverage.line_start);
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 64 * 1024];
        while remaining > 0 {
            let requested = remaining.min(buffer.len() as u64) as usize;
            let read = file.read(&mut buffer[..requested])?;
            if read == 0 {
                return Ok(None);
            }
            hasher.update(&buffer[..read]);
            remaining = remaining.saturating_sub(read as u64);
        }
        if hasher.finalize().to_hex().as_str() != coverage.line_digest {
            return Ok(None);
        }
        Ok(Some(coverage))
    }

    fn contiguous_event_suffix_start(&self, session_id: &str, latest_seq: u64) -> Result<u64> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT seq FROM events WHERE session_id = ? AND seq <= ? ORDER BY seq DESC",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![session_id, i64::try_from(latest_seq).unwrap_or(i64::MAX),],
            |row| row.get::<_, i64>(0),
        )?;
        let mut expected = latest_seq;
        for row in rows {
            let seq = row? as u64;
            if seq != expected {
                break;
            }
            if expected == 0 {
                return Ok(0);
            }
            expected -= 1;
        }
        Ok(expected.saturating_add(1))
    }

    pub fn recover_event_coverage(
        &self,
        session_id: &str,
        events_path: &Path,
    ) -> Result<Option<EventIndexCoverage>> {
        if !self.recover_event_tail(session_id, events_path)? {
            return Ok(None);
        }
        self.validated_event_coverage(session_id, events_path)
    }

    /// Recovers the tail boundary used by timeline pagination without a full
    /// sequence-contiguity scan.
    pub fn recover_timeline_coverage(
        &self,
        session_id: &str,
        events_path: &Path,
    ) -> Result<Option<EventIndexCoverage>> {
        if !self.recover_event_tail(session_id, events_path)? {
            return Ok(None);
        }
        self.validated_timeline_coverage(session_id, events_path)
    }

    fn recover_event_tail(&self, session_id: &str, events_path: &Path) -> Result<bool> {
        if self
            .validated_event_tail(session_id, events_path)?
            .is_some()
        {
            return Ok(true);
        }
        let Some(line) = last_event_log_line(events_path)? else {
            return Ok(false);
        };
        let envelope = match serde_json::from_slice::<crate::event::EventEnvelope>(&line.payload) {
            Ok(envelope) => envelope,
            Err(_) => return Ok(false),
        };
        let indexed_payload = {
            let conn = self.conn();
            let max_seq = conn.query_row(
                "SELECT MAX(seq) FROM events WHERE session_id = ?",
                rusqlite::params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )?;
            if max_seq.map(|seq| seq as u64) != Some(envelope.seq) {
                return Ok(false);
            }
            conn.query_row(
                "SELECT payload FROM events WHERE session_id = ? AND seq = ?",
                rusqlite::params![session_id, i64::try_from(envelope.seq).unwrap_or(i64::MAX)],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        };
        let Some(indexed_payload) = indexed_payload else {
            return Ok(false);
        };
        if indexed_payload.as_bytes() != line.payload {
            return Ok(false);
        }
        let line_digest = blake3::hash(&line.payload).to_hex().to_string();
        {
            let conn = self.conn();
            conn.execute(
                "INSERT INTO event_index_coverage \
                 (session_id, seq, line_start, line_end, log_offset, line_digest) \
                 VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET \
                 seq = excluded.seq, line_start = excluded.line_start, \
                 line_end = excluded.line_end, log_offset = excluded.log_offset, \
                 line_digest = excluded.line_digest",
                rusqlite::params![
                    session_id,
                    i64::try_from(envelope.seq).unwrap_or(i64::MAX),
                    i64::try_from(line.start).unwrap_or(i64::MAX),
                    i64::try_from(line.end).unwrap_or(i64::MAX),
                    i64::try_from(line.log_offset).unwrap_or(i64::MAX),
                    line_digest,
                ],
            )?;
        }
        Ok(true)
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
            let file = match std::fs::File::open(&jsonl) {
                Ok(file) => file,
                Err(_) => continue,
            };
            self.delete_events_for_session(&sid)?;
            let mut reader = std::io::BufReader::new(file);
            let mut offset = 0_u64;
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = reader.read_until(b'\n', &mut line)?;
                if read == 0 {
                    break;
                }
                let line_start = offset;
                offset = offset.saturating_add(read as u64);
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                let Ok(envelope) = serde_json::from_slice::<crate::event::EventEnvelope>(&line)
                else {
                    stats.skipped += 1;
                    continue;
                };
                let payload_json = String::from_utf8_lossy(&line);
                let seq = envelope.seq as i64;
                let ts = envelope.ts.to_rfc3339();
                let (turn_id, flow_run_id) = crate::event_writer::extract_anchors(&envelope.event);
                let text_content =
                    crate::event_writer::extract_text_content(&envelope.event).unwrap_or_default();
                let line_digest = blake3::hash(&line).to_hex().to_string();
                self.insert_project_event_at_boundary(
                    ProjectEventInsert {
                        session_id: &sid,
                        seq,
                        ts: &ts,
                        kind: crate::event_writer::event_kind(&envelope.event),
                        turn_id: turn_id.as_deref(),
                        flow_run_id: flow_run_id.as_deref(),
                        text_content: &text_content,
                        payload_json: &payload_json,
                    },
                    EventLogBoundary {
                        line_start,
                        line_end: line_start.saturating_add(line.len() as u64),
                        log_offset: offset,
                        line_digest: &line_digest,
                    },
                )?;
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

#[derive(Debug, Clone, Copy)]
pub struct EventLogBoundary<'a> {
    pub line_start: u64,
    pub line_end: u64,
    pub log_offset: u64,
    pub line_digest: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventIndexCoverage {
    pub start_seq: u64,
    pub seq: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub log_offset: u64,
    pub line_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTurnRow {
    pub turn_id: String,
    pub start_seq: u64,
    pub latest_seq: u64,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEventSearchHit {
    pub session_id: String,
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub turn_id: Option<String>,
    pub flow_run_id: Option<String>,
    pub payload: String,
    pub text: String,
}

fn project_event_search_hit_from(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ProjectEventSearchHit> {
    Ok(ProjectEventSearchHit {
        session_id: row.get(0)?,
        seq: row.get::<_, i64>(1)? as u64,
        ts: row.get(2)?,
        kind: row.get(3)?,
        turn_id: row.get(4)?,
        flow_run_id: row.get(5)?,
        payload: row.get(6)?,
        text: row.get(7)?,
    })
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

struct EventLogLine {
    start: u64,
    end: u64,
    log_offset: u64,
    payload: Vec<u8>,
}

fn last_event_log_line(path: &Path) -> Result<Option<EventLogLine>> {
    const CHUNK_BYTES: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let log_offset = file.metadata()?.len();
    if log_offset == 0 {
        return Ok(None);
    }
    let mut line_end = log_offset;
    while line_end > 0 {
        file.seek(SeekFrom::Start(line_end - 1))?;
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte)?;
        if !matches!(byte[0], b'\n' | b'\r') {
            break;
        }
        line_end -= 1;
    }
    if line_end == 0 {
        return Ok(None);
    }

    let mut position = line_end;
    let mut reversed_chunks = Vec::new();
    let line_start = loop {
        let start = position.saturating_sub(CHUNK_BYTES);
        let mut chunk = vec![0_u8; (position - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            reversed_chunks.push(chunk[index + 1..].to_vec());
            break start + index as u64 + 1;
        }
        reversed_chunks.push(chunk);
        if start == 0 {
            break 0;
        }
        position = start;
    };
    let mut line = Vec::with_capacity((line_end - line_start) as usize);
    for chunk in reversed_chunks.into_iter().rev() {
        line.extend_from_slice(&chunk);
    }
    Ok(Some(EventLogLine {
        start: line_start,
        end: line_end,
        log_offset,
        payload: line,
    }))
}

fn parse_regex_query(query: &str) -> Option<String> {
    let q = query.trim();
    if q.len() >= 2 && q.starts_with('/') && q.ends_with('/') {
        Some(q[1..q.len() - 1].to_string())
    } else {
        None
    }
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
CREATE INDEX IF NOT EXISTS events_session_seq ON events(session_id, seq);
CREATE INDEX IF NOT EXISTS events_session_turn_seq ON events(session_id, turn_id, seq);

CREATE TABLE IF NOT EXISTS timeline_turns (
    session_id TEXT    NOT NULL,
    turn_id    TEXT    NOT NULL,
    start_seq  INTEGER NOT NULL,
    latest_seq INTEGER NOT NULL,
    PRIMARY KEY (session_id, turn_id)
);
CREATE INDEX IF NOT EXISTS timeline_turns_keyset
ON timeline_turns(session_id, start_seq);

CREATE TABLE IF NOT EXISTS timeline_runs (
    session_id  TEXT NOT NULL,
    flow_run_id TEXT NOT NULL,
    turn_id     TEXT NOT NULL,
    PRIMARY KEY (session_id, flow_run_id)
);

CREATE TABLE IF NOT EXISTS timeline_event_owners (
    session_id TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    turn_id    TEXT    NOT NULL,
    PRIMARY KEY (session_id, seq)
);
CREATE INDEX IF NOT EXISTS timeline_event_owners_turn
ON timeline_event_owners(session_id, turn_id, seq);

CREATE TABLE IF NOT EXISTS timeline_materializations (
    session_id TEXT    PRIMARY KEY,
    source_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS event_index_coverage (
    session_id  TEXT    PRIMARY KEY,
    seq         INTEGER NOT NULL,
    line_start  INTEGER NOT NULL,
    line_end    INTEGER NOT NULL,
    log_offset  INTEGER NOT NULL,
    line_digest TEXT    NOT NULL
);

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
    fn open_project_creates_all_tables() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        let tables = tables_in(&idx);
        for expected in [
            "anchors",
            "confessions",
            "confessions_fts",
            "event_index_coverage",
            "events",
            "events_fts",
            "spec_entries",
            "spec_entries_fts",
            "spec_deviations",
            "spec_deviations_fts",
            "timeline_event_owners",
            "timeline_materializations",
            "timeline_runs",
            "timeline_turns",
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
    fn turn_keyset_is_descending_and_strictly_before_the_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "sess-a", 1, "user_msg", Some("turn-1"), None, "one");
        seed_project_event(
            &idx,
            "sess-a",
            3,
            "assistant_msg",
            Some("turn-1"),
            None,
            "one later",
        );
        seed_project_event(&idx, "sess-a", 4, "user_msg", Some("turn-2"), None, "two");
        seed_project_event(&idx, "sess-a", 8, "user_msg", Some("turn-3"), None, "three");

        let tail = idx.read_turns_before("sess-a", None, 2).unwrap();
        assert_eq!(
            tail,
            vec![
                ProjectTurnRow {
                    turn_id: "turn-3".into(),
                    start_seq: 8,
                    latest_seq: 8,
                },
                ProjectTurnRow {
                    turn_id: "turn-2".into(),
                    start_seq: 4,
                    latest_seq: 4,
                },
            ]
        );
        let before = idx.read_turns_before("sess-a", Some(4), 2).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].turn_id, "turn-1");
        assert_eq!(before[0].latest_seq, 3);
        assert_eq!(idx.count_turns_before("sess-a", None).unwrap(), 3);
        assert_eq!(idx.count_turns_before("sess-a", Some(8)).unwrap(), 2);
        assert_eq!(idx.count_turns_before("sess-a", Some(1)).unwrap(), 0);

        let after = idx.read_turns_after("sess-a", 1, 2).unwrap();
        assert_eq!(
            after,
            vec![
                ProjectTurnRow {
                    turn_id: "turn-2".into(),
                    start_seq: 4,
                    latest_seq: 4,
                },
                ProjectTurnRow {
                    turn_id: "turn-3".into(),
                    start_seq: 8,
                    latest_seq: 8,
                },
            ]
        );
        assert_eq!(idx.read_event_at_seq("sess-a", 4).unwrap().unwrap().seq, 4);
        assert!(idx.read_event_at_seq("sess-a", 99).unwrap().is_none());
    }

    #[test]
    fn run_owned_events_extend_their_turn_without_leaking_interleaved_turns() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(
            &idx,
            "sess-a",
            1,
            "flow_start",
            Some("turn-1"),
            Some("run-1"),
            "",
        );
        seed_project_event(
            &idx,
            "sess-a",
            2,
            "flow_start",
            Some("turn-2"),
            Some("run-2"),
            "",
        );
        seed_project_event(&idx, "sess-a", 3, "flow_node_end", None, Some("run-1"), "");
        seed_project_event(&idx, "sess-a", 4, "flow_node_end", None, Some("run-2"), "");

        let first = idx.read_turns_before("sess-a", Some(2), 1).unwrap();
        assert_eq!(first[0].turn_id, "turn-1");
        assert_eq!(first[0].latest_seq, 3);
        let events = idx.read_events_for_turns("sess-a", &first, None).unwrap();
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(idx.turn_start_for_event("sess-a", 3).unwrap(), Some(1));
        assert_eq!(idx.turn_start_for_event("sess-a", 2).unwrap(), Some(2));
        assert_eq!(idx.turn_start_for_event("sess-a", 99).unwrap(), None);
    }

    #[test]
    fn timeline_materialization_rebuilds_derived_rows_from_existing_events() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(
            &idx,
            "sess-a",
            1,
            "flow_start",
            Some("turn-1"),
            Some("run-1"),
            "",
        );
        seed_project_event(&idx, "sess-a", 2, "flow_node_end", None, Some("run-1"), "");
        {
            let conn = idx.conn();
            conn.execute("DELETE FROM timeline_turns", []).unwrap();
            conn.execute("DELETE FROM timeline_runs", []).unwrap();
            conn.execute("DELETE FROM timeline_event_owners", [])
                .unwrap();
        }

        idx.materialize_timeline_session("sess-a").unwrap();

        let turns = idx.read_turns_before("sess-a", None, 1).unwrap();
        assert_eq!(turns[0].turn_id, "turn-1");
        assert_eq!(turns[0].latest_seq, 2);
        idx.materialize_timeline_session("sess-a").unwrap();
    }

    #[test]
    fn session_backfill_only_inserts_missing_event_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        let turn_id = crate::event::TurnId(uuid::Uuid::from_u128(1));
        let events = vec![
            crate::event::EventEnvelope::new(
                1,
                crate::event::Event::TurnStart {
                    turn_id: turn_id.clone(),
                },
            ),
            crate::event::EventEnvelope::new(
                2,
                crate::event::Event::TurnEnd {
                    turn_id: turn_id.clone(),
                },
            ),
        ];
        let payload = serde_json::to_string(&events[1]).unwrap();
        idx.insert_project_event_raw(ProjectEventInsert {
            session_id: "sess-a",
            seq: 2,
            ts: &events[1].ts.to_rfc3339(),
            kind: "turn_end",
            turn_id: Some(&turn_id.to_string()),
            flow_run_id: None,
            text_content: "",
            payload_json: &payload,
        })
        .unwrap();

        assert_eq!(idx.backfill_session_events("sess-a", &events).unwrap(), 1);
        assert_eq!(idx.backfill_session_events("sess-a", &events).unwrap(), 0);
        assert_eq!(idx.count_events("sess-a", EventFilter::All).unwrap(), 2);
        let turns = idx.read_turns_before("sess-a", None, 1).unwrap();
        assert_eq!(turns[0].start_seq, 1);
        assert_eq!(turns[0].latest_seq, 2);
    }

    #[test]
    fn coverage_requires_the_index_and_jsonl_to_share_one_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let payload = r#"{"type":"user_msg","seq":1}"#;
        let durable = format!("{payload}\n");
        std::fs::write(&events_path, durable.as_bytes()).unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        let digest = blake3::hash(payload.as_bytes()).to_hex().to_string();
        idx.insert_project_event_at_boundary(
            ProjectEventInsert {
                session_id: "sess-a",
                seq: 1,
                ts: "2026-07-05T00:00:00Z",
                kind: "user_msg",
                turn_id: Some("turn-1"),
                flow_run_id: None,
                text_content: "",
                payload_json: payload,
            },
            EventLogBoundary {
                line_start: 0,
                line_end: payload.len() as u64,
                log_offset: durable.len() as u64,
                line_digest: &digest,
            },
        )
        .unwrap();

        assert_eq!(
            idx.validated_event_coverage("sess-a", &events_path)
                .unwrap()
                .unwrap()
                .seq,
            1
        );
        std::fs::write(&events_path, format!("{durable}{{}}\n")).unwrap();
        assert!(
            idx.validated_event_coverage("sess-a", &events_path)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn coverage_can_adopt_an_existing_exact_index_tail() {
        let dir = tempfile::tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let turn_id = crate::event::TurnId(uuid::Uuid::from_u128(1));
        let envelope = crate::event::EventEnvelope::new(
            7,
            crate::event::Event::TurnEnd {
                turn_id: turn_id.clone(),
            },
        );
        let payload = serde_json::to_string(&envelope).unwrap();
        std::fs::write(&events_path, format!("{payload}\n")).unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        idx.insert_project_event_raw(ProjectEventInsert {
            session_id: "sess-a",
            seq: 7,
            ts: &envelope.ts.to_rfc3339(),
            kind: "turn_end",
            turn_id: Some(&turn_id.to_string()),
            flow_run_id: None,
            text_content: "",
            payload_json: &payload,
        })
        .unwrap();

        let coverage = idx
            .recover_event_coverage("sess-a", &events_path)
            .unwrap()
            .unwrap();

        assert_eq!(coverage.start_seq, 7);
        assert_eq!(coverage.seq, 7);
        assert_eq!(coverage.log_offset, payload.len() as u64 + 1);
        assert!(
            idx.validated_event_coverage("sess-a", &events_path)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn timeline_coverage_validates_the_tail_without_scanning_for_internal_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let first = r#"{"type":"turn_start","turn_id":"00000000-0000-0000-0000-000000000001","seq":1,"ts":"2026-07-05T00:00:00Z"}"#;
        let last = r#"{"type":"turn_end","turn_id":"00000000-0000-0000-0000-000000000001","seq":3,"ts":"2026-07-05T00:00:01Z"}"#;
        let durable = format!("{first}\n{last}\n");
        std::fs::write(&events_path, durable.as_bytes()).unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        for (seq, kind, payload) in [(1, "turn_start", first), (3, "turn_end", last)] {
            idx.insert_project_event_raw(ProjectEventInsert {
                session_id: "sess-a",
                seq,
                ts: "2026-07-05T00:00:00Z",
                kind,
                turn_id: Some("00000000-0000-0000-0000-000000000001"),
                flow_run_id: None,
                text_content: "",
                payload_json: payload,
            })
            .unwrap();
        }
        let line_start = first.len() as u64 + 1;
        let digest = blake3::hash(last.as_bytes()).to_hex().to_string();
        idx.insert_project_event_at_boundary(
            ProjectEventInsert {
                session_id: "sess-a",
                seq: 3,
                ts: "2026-07-05T00:00:01Z",
                kind: "turn_end",
                turn_id: Some("00000000-0000-0000-0000-000000000001"),
                flow_run_id: None,
                text_content: "",
                payload_json: last,
            },
            EventLogBoundary {
                line_start,
                line_end: line_start + last.len() as u64,
                log_offset: durable.len() as u64,
                line_digest: &digest,
            },
        )
        .unwrap();

        let timeline = idx
            .validated_timeline_coverage("sess-a", &events_path)
            .unwrap()
            .unwrap();
        let repair = idx
            .validated_event_coverage("sess-a", &events_path)
            .unwrap()
            .unwrap();

        assert_eq!(timeline.start_seq, 1);
        assert_eq!(timeline.seq, 3);
        assert_eq!(repair.start_seq, 3);
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
        assert_eq!(
            idx.count_events("no-such-session", EventFilter::All)
                .unwrap(),
            0
        );
    }

    #[test]
    fn count_events_counts_all_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "hello");
        seed_project_event(&idx, "s1", 2, "assistant_msg", None, None, "hi");
        seed_project_event(&idx, "s1", 3, "tool_result_msg", None, None, "ok");
        seed_project_event(&idx, "s2", 1, "user_msg", None, None, "other");
        assert_eq!(idx.count_events("s1", EventFilter::All).unwrap(), 3);
        assert_eq!(idx.count_events("s2", EventFilter::All).unwrap(), 1);
    }

    #[test]
    fn count_events_filters_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "a");
        seed_project_event(&idx, "s1", 2, "assistant_msg", None, None, "b");
        seed_project_event(&idx, "s1", 3, "tool_result_msg", None, None, "c");
        seed_project_event(&idx, "s1", 4, "user_msg", None, None, "d");
        assert_eq!(
            idx.count_events("s1", EventFilter::Kinds(&["user_msg"]))
                .unwrap(),
            2
        );
        assert_eq!(
            idx.count_events("s1", EventFilter::Kinds(&["user_msg", "assistant_msg"]))
                .unwrap(),
            3
        );
        assert_eq!(
            idx.count_events("s1", EventFilter::Kinds(&["system_msg"]))
                .unwrap(),
            0
        );
    }

    #[test]
    fn read_events_paginated_returns_ordered_by_seq() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        seed_project_event(&idx, "s1", 3, "user_msg", None, None, "third");
        seed_project_event(&idx, "s1", 1, "user_msg", None, None, "first");
        seed_project_event(&idx, "s1", 2, "user_msg", None, None, "second");
        let rows = idx
            .read_events_paginated("s1", 0, 10, EventFilter::All)
            .unwrap();
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
        let rows = idx
            .read_events_paginated("s1", 1, 2, EventFilter::All)
            .unwrap();
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
            .read_events_paginated("s1", 0, 10, EventFilter::Kinds(&["user_msg"]))
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[1].seq, 3);
    }

    #[test]
    fn event_suffix_queries_preserve_reverse_lookup_and_forward_replay_order() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        for seq in 1..=6 {
            let kind = if seq % 2 == 0 {
                "checkpoint"
            } else {
                "user_msg"
            };
            seed_project_event(&idx, "s1", seq, kind, None, None, "");
        }

        let descending = idx
            .read_events_before_descending("s1", Some(6), 2, EventFilter::Kinds(&["checkpoint"]))
            .unwrap();
        assert_eq!(
            descending.iter().map(|row| row.seq).collect::<Vec<_>>(),
            [4, 2]
        );
        let suffix = idx.read_events_from_seq("s1", 4).unwrap();
        assert_eq!(
            suffix.iter().map(|row| row.seq).collect::<Vec<_>>(),
            [4, 5, 6]
        );
        assert!(idx.has_contiguous_event_range("s1", 1, 6).unwrap());
        assert!(!idx.has_contiguous_event_range("s1", 2, 7).unwrap());
    }

    #[test]
    fn read_events_paginated_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let idx = AnchorIndex::open_project(dir.path()).unwrap();
        let rows = idx
            .read_events_paginated("no-such", 0, 10, EventFilter::All)
            .unwrap();
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
