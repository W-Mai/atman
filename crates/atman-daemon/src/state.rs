use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atman_proto::{SessionId, SessionStatus, SessionSummary};

pub struct DaemonState {
    sessions_dir: PathBuf,
}

impl DaemonState {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
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
            let events_path = entry.path().join("events.jsonl");
            let (event_count, first_ts) = summarize_events(&events_path);
            out.push(SessionSummary {
                id: SessionId(uuid),
                event_count,
                first_ts,
                status: SessionStatus::Finished,
            });
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
