//! Cancellable read-only discovery for unbound legacy session pickers.

use std::sync::{Arc, OnceLock};
use tokio::{sync::Semaphore, task::JoinHandle};
use tokio_util::sync::CancellationToken;

static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Default)]
pub(super) struct LegacyDiscovery {
    worker: Option<JoinHandle<Result<Vec<crate::SessionPickerRow>, String>>>,
    cancellation: CancellationToken,
}

impl Drop for LegacyDiscovery {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl LegacyDiscovery {
    pub(super) fn is_waiting(&self) -> bool {
        self.worker.is_some()
    }

    pub(super) fn request(
        &mut self,
        session: Option<Arc<atman_runtime::Session>>,
        scope: super::SessionScope,
    ) -> Result<(), String> {
        self.request_with_limit(
            session,
            scope,
            LIMIT.get_or_init(|| Arc::new(Semaphore::new(2))).clone(),
        )
    }

    fn request_with_limit(
        &mut self,
        session: Option<Arc<atman_runtime::Session>>,
        scope: super::SessionScope,
        limit: Arc<Semaphore>,
    ) -> Result<(), String> {
        self.cancel();
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| error.to_string())?;
        let cancelled = self.cancellation.clone();
        self.worker = Some(runtime.spawn(async move {
            let permit = tokio::select! {
                _ = cancelled.cancelled() => return Ok(Vec::new()),
                permit = limit.acquire_owned() => permit.map_err(|error| error.to_string())?,
            };
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                if cancelled.is_cancelled() {
                    return Ok(Vec::new());
                }
                scan(session, scope, &cancelled).map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())?
        }));
        Ok(())
    }

    pub(super) fn cancel(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
        self.cancellation = CancellationToken::new();
    }

    pub(super) async fn next(&mut self) -> Result<Vec<crate::SessionPickerRow>, String> {
        let Some(worker) = self.worker.as_mut() else {
            return std::future::pending().await;
        };
        let result = worker
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        self.worker = None;
        result
    }
}

fn scan(
    session: Option<Arc<atman_runtime::Session>>,
    scope: crate::session_switcher::SessionScope,
    cancelled: &CancellationToken,
) -> std::io::Result<Vec<crate::SessionPickerRow>> {
    let Some(session) = session else {
        return Ok(Vec::new());
    };
    let session_dir = session.dir();
    let Some(sessions_root) = session_dir.parent() else {
        return Ok(Vec::new());
    };
    let query = match scope {
        crate::session_switcher::SessionScope::All => {
            atman_runtime::session_meta::SessionDiscoveryQuery::all_projects()
        }
        crate::session_switcher::SessionScope::Project => {
            let current_meta = session.meta();
            let Some(project_root) = current_meta
                .as_ref()
                .and_then(|meta| meta.project_root.as_deref())
            else {
                return Ok(Vec::new());
            };
            atman_runtime::session_meta::SessionDiscoveryQuery::current_project(project_root)
        }
    };
    if cancelled.is_cancelled() {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    let entries = std::fs::read_dir(sessions_root)?;
    for entry in entries.flatten() {
        if cancelled.is_cancelled() {
            return Ok(Vec::new());
        }
        if !entry.path().is_dir() {
            continue;
        }
        let sid = entry.file_name().to_string_lossy().to_string();
        let is_current = sid == session.id().to_string();
        let meta = atman_runtime::session_meta::SessionMeta::load(&entry.path());
        if !query.matches_meta(meta.as_ref()) {
            continue;
        }
        let Some(metadata) = meta.as_ref() else {
            continue;
        };
        if cancelled.is_cancelled() {
            return Ok(Vec::new());
        }
        if metadata.project_root.is_none() || metadata.project_fingerprint.is_none() {
            continue;
        }
        let project = metadata
            .project_root
            .as_ref()
            .map(|p| p.display().to_string());
        let events_path = entry.path().join("events.jsonl");
        let updated_at = std::fs::metadata(&events_path)
            .and_then(|m| m.modified())
            .or_else(|_| entry.metadata().and_then(|m| m.modified()))
            .ok()
            .map(|st| {
                let ts: chrono::DateTime<chrono::Local> = st.into();
                ts.to_rfc3339()
            })
            .unwrap_or_default();
        let stats =
            atman_runtime::session_meta::SessionStats::inspect(&entry.path()).unwrap_or_default();
        if cancelled.is_cancelled() {
            return Ok(Vec::new());
        }
        if stats.user_message_count == 0 {
            continue;
        }
        let goal = atman_runtime::memory::goal::GoalStore::at(entry.path())
            .get()
            .ok();
        rows.push(crate::SessionPickerRow {
            selection: None,
            id: sid,
            is_current,
            name: metadata.title.clone(),
            project,
            message_count: stats.message_count as usize,
            updated_at,
            goal,
        });
    }
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rows.truncate(200);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::{
        event::{Event, EventEnvelope},
        message::Message,
        session_meta::SessionMeta,
    };

    fn session(root: &std::path::Path, project: &std::path::Path) -> Arc<atman_runtime::Session> {
        let session = Arc::new(atman_runtime::Session::open(root).unwrap());
        SessionMeta::from_start_path(Some(project))
            .save(session.dir())
            .unwrap();
        let turn = atman_rt::TurnId::now();
        let event = EventEnvelope::new(
            1,
            Event::UserMsg {
                turn_id: turn.clone(),
                flow_run_id: None,
                message: Message::user_text(turn, "input"),
                presentation: None,
                injection_id: None,
            },
        );
        std::fs::write(
            session.dir().join("events.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        session
    }

    #[tokio::test]
    async fn discovery_retains_cancelled_polls_and_publishes_only_the_latest_scope_without_writes()
    {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a");
        let b = directory.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let current = session(directory.path(), &a);
        let other = session(directory.path(), &b);
        let files = |session: &atman_runtime::Session| {
            let mut names = std::fs::read_dir(session.dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let before = (files(&current), files(&other));
        let gate = Arc::new(Semaphore::new(0));
        let mut discovery = LegacyDiscovery::default();
        discovery
            .request_with_limit(
                Some(current.clone()),
                super::super::SessionScope::Project,
                gate.clone(),
            )
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), discovery.next())
                .await
                .is_err()
        );
        assert!(discovery.is_waiting());
        discovery
            .request_with_limit(
                Some(current.clone()),
                super::super::SessionScope::All,
                gate.clone(),
            )
            .unwrap();
        gate.add_permits(1);
        let rows = discovery.next().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().filter(|row| row.is_current).count(), 1);
        let mut picker = super::super::SessionSwitcher::default();
        picker.open_with(rows, super::super::SessionScope::All);
        picker.selected = picker
            .rows
            .iter()
            .position(|row| row.id == other.id().to_string())
            .unwrap();
        let app = crate::app::AppState::new(current.id().to_string(), None)
            .with_session(Some(current.clone()));
        picker.refresh_legacy(&app);
        picker.toggle_sort();
        picker.next_legacy_update().await;
        assert_eq!(picker.selected_id(), Some(other.id().to_string()));
        picker.close();
        assert!(!picker.has_legacy_load());
        discovery
            .request_with_limit(
                Some(current.clone()),
                super::super::SessionScope::Project,
                gate.clone(),
            )
            .unwrap();
        assert_eq!(
            discovery.next().await.unwrap()[0].id,
            current.id().to_string()
        );
        assert_eq!((files(&current), files(&other)), before);
        let held = gate.acquire().await.unwrap();
        discovery
            .request_with_limit(Some(current), super::super::SessionScope::All, gate.clone())
            .unwrap();
        discovery.cancel();
        assert!(!discovery.is_waiting());
        drop(held);
    }
}
