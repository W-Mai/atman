//! Abstract history store — tools call this trait, never touching storage directly.
//!
//! `HistoryStoreImpl` routes internally: SQLite (priority) → messages_full
//! (fallback) → events.jsonl replay (last resort).

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::RuntimeError;
use crate::event::EventEnvelope;
use crate::index::AnchorIndex;
use crate::message::Message;
use crate::projection::message_window::replay_all_messages_with_seq;
use crate::session::SessionOpenError;

pub enum SearchScope {
    Session,
    Project,
}

pub struct HistoryQuery {
    pub session_id: String,
    pub offset: usize,
    pub limit: usize,
    pub role_filter: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct HistoryPage {
    pub total: u64,
    pub offset: usize,
    pub limit: usize,
    pub items: Vec<Message>,
}

#[derive(Debug)]
pub struct SearchHit {
    pub session_id: String,
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub snippet: String,
}

#[derive(Debug)]
pub struct SearchResult {
    pub total: u64,
    pub hits: Vec<SearchHit>,
}

pub trait HistoryStore: Send + Sync {
    fn count(&self, session_id: &str, role_filter: Option<&[&str]>) -> Result<u64, RuntimeError>;
    fn read(&self, query: HistoryQuery) -> Result<HistoryPage, RuntimeError>;
    fn search(
        &self,
        query: &str,
        scope: SearchScope,
        limit: usize,
    ) -> Result<SearchResult, RuntimeError>;
    fn recent(&self, n: usize) -> Result<(u64, Vec<Message>), RuntimeError>;
}

pub struct HistoryStoreImpl {
    project_index: Option<Arc<AnchorIndex>>,
    session: Option<Arc<crate::session::Session>>,
    current_session_id: Option<String>,
    sessions_root: Option<PathBuf>,
}

impl HistoryStoreImpl {
    pub fn new(
        project_index: Option<Arc<AnchorIndex>>,
        session: Option<Arc<crate::session::Session>>,
        current_session_id: Option<String>,
        sessions_root: Option<PathBuf>,
    ) -> Self {
        Self {
            project_index,
            session,
            current_session_id,
            sessions_root,
        }
    }

    fn is_current_session(&self, session_id: &str) -> bool {
        self.current_session_id
            .as_deref()
            .is_some_and(|sid| sid == session_id)
    }

    fn sqlite_available(&self) -> bool {
        self.project_index.is_some()
    }

    fn session_dir(&self, session_id: &str) -> Result<PathBuf, RuntimeError> {
        self.sessions_root
            .as_ref()
            .map(|root| root.join(session_id))
            .ok_or_else(|| {
                RuntimeError::ToolFailed(format!(
                    "history: no sessions_root to resolve session `{session_id}`"
                ))
            })
    }

    fn replay_from_jsonl(
        &self,
        session_id: &str,
        role_filter: Option<&[&str]>,
    ) -> Result<Vec<Message>, RuntimeError> {
        let dir = self.session_dir(session_id)?;
        let path = dir.join("events.jsonl");
        let msgs = replay_all_messages_with_seq(&path).map_err(|e| {
            RuntimeError::ToolFailed(format!("history: replay {}: {e}", path.display()))
        })?;
        let msgs: Vec<Message> = msgs.into_iter().map(|(_, m)| m).collect();
        Ok(filter_messages_by_role(msgs, role_filter))
    }
}

fn role_to_kind(role: &str) -> Option<&'static str> {
    match role {
        "user" => Some("user_msg"),
        "assistant" => Some("assistant_msg"),
        "tool" => Some("tool_result_msg"),
        "system" => Some("system_msg"),
        _ => None,
    }
}

fn roles_to_kinds(roles: Option<&[&str]>) -> Option<Vec<&'static str>> {
    roles.map(|rs| {
        rs.iter()
            .filter_map(|r| role_to_kind(r))
            .collect::<Vec<_>>()
    })
}

fn filter_messages_by_role(msgs: Vec<Message>, roles: Option<&[&str]>) -> Vec<Message> {
    match roles {
        Some(rs) if !rs.is_empty() => msgs
            .into_iter()
            .filter(|m| rs.iter().any(|r| *r == m.role.as_str()))
            .collect(),
        _ => msgs,
    }
}

fn extract_message_from_payload(payload: &str) -> Option<Message> {
    let env: EventEnvelope = serde_json::from_str(payload).ok()?;
    match env.event {
        crate::event::Event::UserMsg { message, .. }
        | crate::event::Event::AssistantMsg { message, .. }
        | crate::event::Event::ToolResultMsg { message, .. }
        | crate::event::Event::SystemMsg { message, .. } => Some(message),
        _ => None,
    }
}

fn rows_to_messages(rows: Vec<crate::index::ProjectEventRow>) -> Vec<Message> {
    rows.into_iter()
        .filter_map(|r| extract_message_from_payload(&r.payload))
        .collect()
}

impl HistoryStore for HistoryStoreImpl {
    fn count(&self, session_id: &str, role_filter: Option<&[&str]>) -> Result<u64, RuntimeError> {
        let kinds = roles_to_kinds(role_filter);

        if let Some(idx) = &self.project_index
            && self.sqlite_available()
        {
            return idx
                .count_events(session_id, kinds.as_deref())
                .map_err(|e| RuntimeError::ToolFailed(format!("history.count: {e}")));
        }

        if self.is_current_session(session_id)
            && let Some(session) = &self.session
        {
            let msgs = session.messages_full();
            let filtered = filter_messages_by_role(msgs.to_vec(), role_filter);
            return Ok(filtered.len() as u64);
        }

        let msgs = self.replay_from_jsonl(session_id, role_filter)?;
        Ok(msgs.len() as u64)
    }

    fn read(&self, query: HistoryQuery) -> Result<HistoryPage, RuntimeError> {
        let HistoryQuery {
            session_id,
            offset,
            limit,
            role_filter,
        } = query;
        let role_strs: Option<Vec<&str>> = role_filter
            .as_ref()
            .map(|rs| rs.iter().map(|s| s.as_str()).collect());
        let kinds = roles_to_kinds(role_strs.as_deref());
        let offset0 = offset.saturating_sub(1);

        if let Some(idx) = &self.project_index
            && self.sqlite_available()
        {
            let total = idx
                .count_events(&session_id, kinds.as_deref())
                .map_err(|e| RuntimeError::ToolFailed(format!("history.read count: {e}")))?;
            let rows = idx
                .read_events_paginated(&session_id, offset0, limit, kinds.as_deref())
                .map_err(|e| RuntimeError::ToolFailed(format!("history.read: {e}")))?;
            let items = rows_to_messages(rows);
            return Ok(HistoryPage {
                total,
                offset,
                limit,
                items,
            });
        }

        let msgs = if self.is_current_session(&session_id)
            && let Some(session) = &self.session
        {
            session.messages_full().to_vec()
        } else {
            self.replay_from_jsonl(&session_id, None)?
        };
        let filtered = filter_messages_by_role(msgs, role_strs.as_deref());
        let total = filtered.len() as u64;
        let end = (offset0 + limit).min(filtered.len());
        let items = if offset0 >= filtered.len() {
            Vec::new()
        } else {
            filtered[offset0..end].to_vec()
        };
        Ok(HistoryPage {
            total,
            offset,
            limit,
            items,
        })
    }

    fn search(
        &self,
        query: &str,
        scope: SearchScope,
        limit: usize,
    ) -> Result<SearchResult, RuntimeError> {
        if query.trim().is_empty() {
            return Err(RuntimeError::ToolFailed(
                "history.search: empty query".into(),
            ));
        }

        let session_filter = match &scope {
            SearchScope::Project => None,
            SearchScope::Session => self.current_session_id.clone(),
        };

        if let Some(idx) = &self.project_index {
            let total = idx
                .count_search_hits(query, session_filter.as_deref())
                .map_err(|e| RuntimeError::ToolFailed(format!("history.search count: {e}")))?;
            let rows = idx
                .fts_search_project_events(query, session_filter.as_deref(), limit)
                .map_err(|e| RuntimeError::ToolFailed(format!("history.search: {e}")))?;
            let hits = rows
                .into_iter()
                .map(|r| {
                    let snippet: String = r
                        .payload
                        .chars()
                        .take(200)
                        .collect::<String>()
                        .replace('\n', " ");
                    SearchHit {
                        session_id: r.session_id,
                        seq: r.seq,
                        ts: r.ts,
                        kind: r.kind,
                        snippet,
                    }
                })
                .collect();
            return Ok(SearchResult { total, hits });
        }

        if matches!(scope, SearchScope::Project) {
            return Err(RuntimeError::ToolFailed(
                "history.search: project scope requires project index".into(),
            ));
        }

        if let Some(session) = &self.session {
            let msgs = session.messages_full();
            let query_lower = query.to_lowercase();
            let mut hits = Vec::new();
            let sid = self.current_session_id.clone().unwrap_or_default();
            for (i, msg) in msgs.iter().enumerate() {
                let text = msg.text_concat();
                if text.to_lowercase().contains(&query_lower) {
                    let snippet: String = text.chars().take(200).collect();
                    hits.push(SearchHit {
                        session_id: sid.clone(),
                        seq: i as u64,
                        ts: String::new(),
                        kind: msg.role.as_str().to_string(),
                        snippet,
                    });
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
            let total = hits.len() as u64;
            return Ok(SearchResult { total, hits });
        }

        Err(RuntimeError::ToolFailed(
            "history.search: no project index or session on context".into(),
        ))
    }

    fn recent(&self, n: usize) -> Result<(u64, Vec<Message>), RuntimeError> {
        if let Some(session) = &self.session {
            let msgs = session.messages_full();
            let total = msgs.len() as u64;
            if n == 0 {
                return Ok((total, Vec::new()));
            }
            let start = msgs.len().saturating_sub(n);
            let items = msgs[start..].to_vec();
            return Ok((total, items));
        }

        if let Some(session_id) = &self.current_session_id {
            let msgs = self.replay_from_jsonl(session_id, None)?;
            let total = msgs.len() as u64;
            if n == 0 {
                return Ok((total, Vec::new()));
            }
            let start = msgs.len().saturating_sub(n);
            let items = msgs[start..].to_vec();
            return Ok((total, items));
        }

        Err(RuntimeError::ToolFailed(
            "memory.recent_turns: no session available".into(),
        ))
    }
}

impl From<SessionOpenError> for RuntimeError {
    fn from(e: SessionOpenError) -> Self {
        RuntimeError::ToolFailed(format!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{AnchorIndex, ProjectEventInsert};
    use crate::message::{Message, MessagePart, MessageRole};
    use tempfile::TempDir;

    fn user_msg(text: &str) -> Message {
        Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
            }],
            turn_id: crate::event::TurnId::now(),
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
            }],
            turn_id: crate::event::TurnId::now(),
        }
    }

    fn seed(idx: &AnchorIndex, sid: &str, seq: i64, kind: &str, text: &str) {
        let payload = serde_json::json!({
            "type": kind,
            "seq": seq,
            "turn_id": "019f0000-0000-7000-0000-000000000001",
            "message": {
                "role": if kind == "user_msg" { "user" } else { "assistant" },
                "parts": [{"type": "text", "text": text}],
                "turn_id": "019f0000-0000-7000-0000-000000000001"
            },
            "ts": "2026-07-08T00:00:00Z"
        });
        idx.insert_project_event_raw(ProjectEventInsert {
            session_id: sid,
            seq,
            ts: "2026-07-08T00:00:00Z",
            kind,
            turn_id: Some("019f0000-0000-7000-0000-000000000001"),
            flow_run_id: None,
            text_content: text,
            payload_json: &payload.to_string(),
        })
        .unwrap();
    }

    #[test]
    fn count_via_sqlite() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, Some("s1".into()), None);
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            1,
            "user_msg",
            "hello",
        );
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            2,
            "assistant_msg",
            "hi",
        );
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            3,
            "user_msg",
            "bye",
        );
        assert_eq!(store.count("s1", None).unwrap(), 3);
        assert_eq!(store.count("s1", Some(&["user"])).unwrap(), 2);
        assert_eq!(store.count("s1", Some(&["assistant"])).unwrap(), 1);
    }

    #[test]
    fn read_via_sqlite_paginated() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, Some("s1".into()), None);
        for i in 1..=5 {
            seed(
                store.project_index.as_ref().unwrap(),
                "s1",
                i,
                "user_msg",
                &format!("msg {i}"),
            );
        }
        let page = store
            .read(HistoryQuery {
                session_id: "s1".into(),
                offset: 2,
                limit: 2,
                role_filter: None,
            })
            .unwrap();
        assert_eq!(page.total, 5);
        assert_eq!(page.offset, 2);
        assert_eq!(page.limit, 2);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].text_concat(), "msg 2");
        assert_eq!(page.items[1].text_concat(), "msg 3");
    }

    #[test]
    fn read_via_sqlite_role_filter() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, Some("s1".into()), None);
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            1,
            "user_msg",
            "u1",
        );
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            2,
            "assistant_msg",
            "a1",
        );
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            3,
            "user_msg",
            "u2",
        );
        let page = store
            .read(HistoryQuery {
                session_id: "s1".into(),
                offset: 1,
                limit: 100,
                role_filter: Some(vec!["user".into()]),
            })
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].text_concat(), "u1");
        assert_eq!(page.items[1].text_concat(), "u2");
    }

    #[test]
    fn search_returns_total_and_hits() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, Some("s1".into()), None);
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            1,
            "user_msg",
            "hello world",
        );
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            2,
            "user_msg",
            "hello again",
        );
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            3,
            "user_msg",
            "goodbye",
        );
        let result = store.search("hello", SearchScope::Session, 10).unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(result.hits.len(), 2);
    }

    #[test]
    fn search_cjk() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, Some("s1".into()), None);
        seed(
            store.project_index.as_ref().unwrap(),
            "s1",
            1,
            "user_msg",
            "浮动面板设计",
        );
        let result = store.search("浮动", SearchScope::Session, 10).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.hits.len(), 1);
    }

    #[test]
    fn count_empty_session_via_sqlite() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, Some("s1".into()), None);
        assert_eq!(store.count("no-such", None).unwrap(), 0);
    }

    #[test]
    fn read_empty_session_via_sqlite() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, Some("s1".into()), None);
        let page = store
            .read(HistoryQuery {
                session_id: "no-such".into(),
                offset: 1,
                limit: 10,
                role_filter: None,
            })
            .unwrap();
        assert_eq!(page.total, 0);
        assert!(page.items.is_empty());
    }

    // Fallback: no project_index, but session has messages_full
    // We can't easily construct a Session in unit tests without a tempdir + events.jsonl,
    // so the messages_full fallback path is covered by integration tests.
    // Here we test the jsonl replay fallback (no session, no index, but sessions_root points to jsonl).

    #[test]
    fn fallback_replay_from_jsonl() {
        let dir = TempDir::new().unwrap();
        let sessions_root = dir.path().join("sessions");
        let sid = "test-sid";
        let session_dir = sessions_root.join(sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        let events_path = session_dir.join("events.jsonl");
        let u1 = user_msg("first");
        let a1 = assistant_msg("second");
        let env1 = crate::event::EventEnvelope::new(
            1,
            crate::event::Event::UserMsg {
                turn_id: u1.turn_id.clone(),
                message: u1,
            },
        );
        let env2 = crate::event::EventEnvelope::new(
            2,
            crate::event::Event::AssistantMsg {
                turn_id: a1.turn_id.clone(),
                flow_run_id: None,
                message: a1,
            },
        );
        let line1 = serde_json::to_string(&env1).unwrap();
        let line2 = serde_json::to_string(&env2).unwrap();
        std::fs::write(&events_path, format!("{line1}\n{line2}\n")).unwrap();

        let store = HistoryStoreImpl::new(None, None, Some(sid.into()), Some(sessions_root));
        assert_eq!(store.count(sid, None).unwrap(), 2);
        assert_eq!(store.count(sid, Some(&["user"])).unwrap(), 1);
        assert_eq!(store.count(sid, Some(&["assistant"])).unwrap(), 1);

        let page = store
            .read(HistoryQuery {
                session_id: sid.into(),
                offset: 1,
                limit: 10,
                role_filter: None,
            })
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].text_concat(), "first");
        assert_eq!(page.items[1].text_concat(), "second");
    }

    #[test]
    fn fallback_replay_paginated() {
        let dir = TempDir::new().unwrap();
        let sessions_root = dir.path().join("sessions");
        let sid = "test-sid";
        let session_dir = sessions_root.join(sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        let events_path = session_dir.join("events.jsonl");
        let mut lines = Vec::new();
        for i in 1..=5 {
            let m = user_msg(&format!("msg {i}"));
            let env = crate::event::EventEnvelope::new(
                i,
                crate::event::Event::UserMsg {
                    turn_id: m.turn_id.clone(),
                    message: m,
                },
            );
            lines.push(serde_json::to_string(&env).unwrap());
        }
        std::fs::write(&events_path, lines.join("\n") + "\n").unwrap();

        let store = HistoryStoreImpl::new(None, None, Some(sid.into()), Some(sessions_root));
        let page = store
            .read(HistoryQuery {
                session_id: sid.into(),
                offset: 2,
                limit: 2,
                role_filter: None,
            })
            .unwrap();
        assert_eq!(page.total, 5);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].text_concat(), "msg 2");
        assert_eq!(page.items[1].text_concat(), "msg 3");
    }

    #[test]
    fn search_empty_query_errors() {
        let dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        let store = HistoryStoreImpl::new(Some(idx), None, None, None);
        let err = store.search("", SearchScope::Session, 10).unwrap_err();
        assert!(matches!(err, RuntimeError::ToolFailed(_)));
    }

    #[test]
    fn search_project_scope_without_index_errors() {
        let store = HistoryStoreImpl::new(None, None, None, None);
        let err = store.search("hello", SearchScope::Project, 10).unwrap_err();
        assert!(matches!(err, RuntimeError::ToolFailed(_)));
    }
}
