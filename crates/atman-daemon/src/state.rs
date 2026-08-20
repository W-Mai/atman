use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId, PromptId, SessionId, SessionStatus, SessionSummary};
use atman_runtime::event::{Event, EventSink};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct PendingPrompt {
    tx: oneshot::Sender<serde_json::Value>,
    broadcast_sink: Option<EventSink>,
}

pub struct DaemonState {
    data_dir: PathBuf,
    live: Mutex<HashMap<SessionId, LiveSession>>,
    prompts: Mutex<HashMap<PromptId, PendingPrompt>>,
    launcher: Mutex<Option<std::sync::Arc<crate::run::RunLauncher>>>,
}

pub struct LiveSession {
    pub run_id: FlowRunId,
    pub flow_name: String,
    pub cancel: CancellationToken,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl DaemonState {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            live: Mutex::new(HashMap::new()),
            prompts: Mutex::new(HashMap::new()),
            launcher: Mutex::new(None),
        }
    }

    pub fn set_launcher(&self, launcher: std::sync::Arc<crate::run::RunLauncher>) {
        *self.launcher.lock().unwrap() = Some(launcher);
    }

    pub fn launcher(&self) -> Option<std::sync::Arc<crate::run::RunLauncher>> {
        self.launcher.lock().unwrap().clone()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn register_pending_prompt(&self, id: PromptId) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.prompts.lock().unwrap().insert(
            id,
            PendingPrompt {
                tx,
                broadcast_sink: None,
            },
        );
        rx
    }

    pub fn register_pending_prompt_broadcast(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
        sink: EventSink,
    ) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.prompts.lock().unwrap().insert(
            id.clone(),
            PendingPrompt {
                tx,
                broadcast_sink: Some(sink.clone()),
            },
        );
        sink.emit(Event::PendingPrompt {
            prompt_id: id.0,
            kind: kind.to_string(),
            payload,
        });
        rx
    }

    pub fn resolve_prompt(&self, id: &PromptId, answer: serde_json::Value) -> bool {
        let Some(entry) = self.prompts.lock().unwrap().remove(id) else {
            return false;
        };
        if let Some(sink) = &entry.broadcast_sink {
            sink.emit(Event::PromptResolved {
                prompt_id: id.0,
                answer: answer.clone(),
            });
        }
        entry.tx.send(answer).is_ok()
    }

    pub fn drop_pending_prompt(&self, id: &PromptId) {
        if let Some(entry) = self.prompts.lock().unwrap().remove(id)
            && let Some(sink) = &entry.broadcast_sink
        {
            sink.emit(Event::PromptResolved {
                prompt_id: id.0,
                answer: serde_json::Value::Null,
            });
        }
    }

    pub fn pending_prompt_ids(&self) -> Vec<PromptId> {
        self.prompts.lock().unwrap().keys().cloned().collect()
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
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

    pub fn rename_session(&self, sid: &SessionId, title: &str) -> Result<SessionSummary> {
        let path = self.sessions_root().join(sid.0.to_string());
        atman_runtime::session_meta::SessionMeta::rename(&path, title)
            .with_context(|| format!("rename session {sid}"))?;
        self.list_sessions()?
            .into_iter()
            .find(|summary| &summary.id == sid)
            .ok_or_else(|| anyhow::anyhow!("session not found: {sid}"))
    }

    pub fn list_sessions_query(
        &self,
        project_root: Option<&str>,
        search: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionSummary>> {
        let mut summaries = self.list_sessions()?;
        if let Some(project_root) = project_root {
            let query = atman_runtime::session_meta::SessionDiscoveryQuery::current_project(
                std::path::Path::new(project_root),
            );
            let sessions_root = self.sessions_root();
            summaries.retain(|summary| {
                let path = sessions_root.join(summary.id.to_string());
                query.matches_meta(atman_runtime::session_meta::SessionMeta::load(&path).as_ref())
            });
        }
        if let Some(search) = search.map(str::trim).filter(|value| !value.is_empty()) {
            let needle = search.to_lowercase();
            summaries.retain(|summary| {
                summary.id.to_string().to_lowercase().contains(&needle)
                    || summary.title.to_lowercase().contains(&needle)
                    || summary
                        .project_root
                        .as_ref()
                        .is_some_and(|root| root.to_lowercase().contains(&needle))
            });
        }
        if let Some(limit) = limit {
            summaries.truncate(limit);
        }
        Ok(summaries)
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

        let sessions_root = self.sessions_root();
        if sessions_root.exists() {
            for entry in std::fs::read_dir(&sessions_root)
                .with_context(|| format!("read_dir {}", sessions_root.display()))?
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
                let meta = atman_runtime::session_meta::SessionMeta::load(&entry.path());
                out.push(SessionSummary {
                    id: sid.clone(),
                    event_count,
                    first_ts,
                    status,
                    title: meta
                        .as_ref()
                        .and_then(|m| m.title.clone())
                        .unwrap_or_else(|| "Untitled session".into()),
                    goal: atman_runtime::memory::goal::GoalStore::at(entry.path())
                        .get()
                        .ok(),
                    project_root: meta
                        .as_ref()
                        .and_then(|m| m.project_root.as_ref())
                        .map(|p| p.display().to_string()),
                    name_source: match meta
                        .as_ref()
                        .map(|metadata| metadata.name_source)
                        .unwrap_or_default()
                    {
                        atman_runtime::session_meta::NameSource::Auto => {
                            atman_proto::NameSource::Auto
                        }
                        atman_runtime::session_meta::NameSource::User => {
                            atman_proto::NameSource::User
                        }
                    },
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
                    title: "Untitled session".into(),
                    goal: None,
                    project_root: None,
                    name_source: atman_proto::NameSource::Auto,
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
