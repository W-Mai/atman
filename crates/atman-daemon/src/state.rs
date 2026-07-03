use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId, SessionId, SessionStatus, SessionSummary};
use tokio_util::sync::CancellationToken;

pub struct DaemonState {
    sessions_dir: PathBuf,
    live: Mutex<HashMap<SessionId, LiveSession>>,
}

pub struct LiveSession {
    pub run_id: FlowRunId,
    pub flow_name: String,
    pub cancel: CancellationToken,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl DaemonState {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self {
            sessions_dir,
            live: Mutex::new(HashMap::new()),
        }
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn register_live(&self, id: SessionId, entry: LiveSession) {
        self.live.lock().unwrap().insert(id, entry);
    }

    pub fn deregister_live(&self, id: &SessionId) {
        self.live.lock().unwrap().remove(id);
    }

    pub fn cancel_run(&self, run_id: &FlowRunId) -> bool {
        let live = self.live.lock().unwrap();
        for entry in live.values() {
            if &entry.run_id == run_id {
                entry.cancel.cancel();
                return true;
            }
        }
        false
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let live_ids: HashMap<SessionId, chrono::DateTime<chrono::Utc>> = {
            let live = self.live.lock().unwrap();
            live.iter()
                .map(|(sid, entry)| (sid.clone(), entry.started_at))
                .collect()
        };

        let mut out: Vec<SessionSummary> = Vec::new();
        let mut seen: std::collections::HashSet<SessionId> = std::collections::HashSet::new();

        if self.sessions_dir.exists() {
            for entry in std::fs::read_dir(&self.sessions_dir)
                .with_context(|| format!("read_dir {}", self.sessions_dir.display()))?
            {
                let entry = entry?;
                if !entry.path().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Ok(uuid) = uuid::Uuid::parse_str(&name) else {
                    continue;
                };
                let sid = SessionId(uuid);
                let events_path = entry.path().join("events.jsonl");
                let (event_count, first_ts) = summarize_events(&events_path);
                let status = if live_ids.contains_key(&sid) {
                    SessionStatus::Running
                } else {
                    SessionStatus::Finished
                };
                out.push(SessionSummary {
                    id: sid.clone(),
                    event_count,
                    first_ts,
                    status,
                });
                seen.insert(sid);
            }
        }
        for (sid, started_at) in &live_ids {
            if !seen.contains(sid) {
                out.push(SessionSummary {
                    id: sid.clone(),
                    event_count: 0,
                    first_ts: Some(*started_at),
                    status: SessionStatus::Running,
                });
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.id.0));
        Ok(out)
    }
}

fn summarize_events(path: &Path) -> (usize, Option<chrono::DateTime<chrono::Utc>>) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return (0, None);
    };
    let mut count = 0usize;
    let mut first_ts = None;
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        if first_ts.is_none()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(ts) = v.get("ts").and_then(|t| t.as_str())
            && let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts)
        {
            first_ts = Some(parsed.with_timezone(&chrono::Utc));
        }
    }
    (count, first_ts)
}
