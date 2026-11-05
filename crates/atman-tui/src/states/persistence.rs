//! Ordered preference I/O; UI readers never share a lock with disk work.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::watch;

use super::PersistedUiState;

// Scalar fields use a fixed bit; panel dimensions merge independently by key.
macro_rules! fields {
    ($visit:ident) => { $visit!(theme:0, sidebar_mode:1, sidebar_visible:2,
        sidebar_collapse_locked:3, mouse_captured:4, goal_collapsed:5,
        plan_collapsed:6, todo_collapsed:7, context_collapsed:8, meta_collapsed:9,
        sidebar_upper_collapsed:10, sidebar_lower_collapsed:11,
        task_panel_collapsed:12, onboarding_skipped:13, hints_dismissed:14); };
}

#[derive(Clone, Default)]
struct Patch {
    values: PersistedUiState,
    mask: u64,
    panels: HashMap<String, Option<(u16, u16)>>,
    revision: u64,
}

impl Patch {
    fn merge(&mut self, patch: Self) {
        macro_rules! merge_fields { ($($name:ident:$bit:literal),*) => { $(
            if patch.mask & (1 << $bit) != 0 { self.values.$name = patch.values.$name; }
        )* }; }
        fields!(merge_fields);
        self.mask |= patch.mask;
        self.panels.extend(patch.panels);
    }
    fn record(&mut self, before: &mut PersistedUiState, app: &crate::app::AppState) -> bool {
        let after = PersistedUiState::snapshot_fields(app);
        let mut changed = false;
        macro_rules! record_fields { ($($name:ident:$bit:literal),*) => { $(
            if before.$name != after.$name {
                self.values.$name = after.$name;
                before.$name = after.$name;
                self.mask |= 1 << $bit;
                changed = true;
            }
        )* }; }
        fields!(record_fields);
        let keys = before
            .panel_sizes
            .keys()
            .chain(app.panel_sizes.keys())
            .filter(|key| before.panel_sizes.get(*key) != app.panel_sizes.get(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            let value = app.panel_sizes.get(&key).copied();
            self.panels.insert(key.clone(), value);
            match value {
                Some(value) => {
                    before.panel_sizes.insert(key, value);
                }
                None => {
                    before.panel_sizes.remove(&key);
                }
            }
            changed = true;
        }
        changed
    }

    fn apply(self, state: &mut PersistedUiState) {
        macro_rules! apply_fields { ($($name:ident:$bit:literal),*) => { $(
            if self.mask & (1 << $bit) != 0 { state.$name = self.values.$name; }
        )* }; }
        fields!(apply_fields);
        for (key, value) in self.panels {
            match value {
                Some(value) => {
                    state.panel_sizes.insert(key, value);
                }
                None => {
                    state.panel_sizes.remove(&key);
                }
            }
        }
    }
}

#[derive(Clone, Default)]
struct Receipt {
    state: Option<Arc<PersistedUiState>>,
    attempted: u64,
    error: Option<String>,
}

struct Queue {
    pending: Mutex<Patch>,
    desired: AtomicU64,
    since: Mutex<Option<Instant>>,
}

#[derive(Clone)]
pub(crate) struct PreferenceStore {
    queue: Arc<Queue>,
    wake: watch::Sender<()>,
    receipt: watch::Receiver<Receipt>,
}

pub(crate) struct PreferenceBinding {
    pub store: PreferenceStore,
    observed: Mutex<PersistedUiState>,
    initial_overlay: Mutex<Patch>,
    loaded: bool,
    reported_error: Option<u64>,
}

impl PreferenceStore {
    pub fn start() -> Self {
        Self::start_with_path(PersistedUiState::path)
    }

    fn start_with_path(resolve: impl FnOnce() -> Option<PathBuf> + Send + 'static) -> Self {
        let queue = Arc::new(Queue {
            pending: Mutex::new(Patch::default()),
            desired: AtomicU64::new(0),
            since: Mutex::new(Some(Instant::now())),
        });
        let (wake, mut requests) = watch::channel(());
        let (receipts, receipt) = watch::channel(Receipt::default());
        let pending = queue.clone();
        tokio::spawn(async move {
            let loaded = tokio::task::spawn_blocking(move || {
                let path = resolve();
                let state = path
                    .as_deref()
                    .map(PersistedUiState::load_from)
                    .unwrap_or_default();
                (path, state)
            })
            .await;
            let (path, state) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    receipts.send_replace(Receipt {
                        state: Some(Arc::new(PersistedUiState::default())),
                        attempted: pending.desired.load(Ordering::Acquire),
                        error: Some(format!("UI preference loading stopped: {error}")),
                    });
                    return;
                }
            };
            let first = std::mem::take(&mut *pending.pending.lock().unwrap());
            let mut revision = first.revision;
            let mut state = Arc::new(state);
            state = match apply_patch(state.clone(), first).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    receipts.send_replace(Receipt {
                        state: Some(state),
                        attempted: pending.desired.load(Ordering::Acquire),
                        error: Some(error),
                    });
                    return;
                }
            };
            receipts.send_replace(Receipt {
                state: Some(state.clone()),
                ..Receipt::default()
            });
            loop {
                if revision > receipts.borrow().attempted {
                    let snapshot = state.clone();
                    let destination = path.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let path = destination
                            .ok_or_else(|| anyhow::anyhow!("UI preference path is unavailable"))?;
                        snapshot.save_to(&path)
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()));
                    receipts.send_replace(Receipt {
                        state: Some(state.clone()),
                        attempted: revision,
                        error: result.err(),
                    });
                }
                let patch = std::mem::take(&mut *pending.pending.lock().unwrap());
                if patch.revision > revision {
                    revision = patch.revision;
                    state = match apply_patch(state.clone(), patch).await {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            receipts.send_replace(Receipt {
                                state: Some(state),
                                attempted: pending.desired.load(Ordering::Acquire),
                                error: Some(error),
                            });
                            return;
                        }
                    };
                    continue;
                }
                {
                    let mut since = pending.since.lock().unwrap();
                    if pending.desired.load(Ordering::Acquire) <= receipts.borrow().attempted {
                        *since = None;
                    }
                }
                if requests.changed().await.is_err() {
                    break;
                }
            }
        });
        Self {
            queue,
            wake,
            receipt,
        }
    }

    pub fn bind(&self, app: &mut crate::app::AppState) -> PreferenceBinding {
        let receipt = self.receipt.borrow().clone();
        let state = receipt.state.as_deref().cloned().unwrap_or_default();
        state.apply(app);
        PreferenceBinding {
            store: self.clone(),
            observed: Mutex::new(state),
            initial_overlay: Mutex::new(Patch::default()),
            loaded: receipt.state.is_some(),
            reported_error: None,
        }
    }

    pub fn is_waiting(&self) -> bool {
        if self.receipt.has_changed().is_err() {
            return false;
        }
        let receipt = self.receipt.borrow();
        receipt.state.is_none() || receipt.attempted < self.queue.desired.load(Ordering::Acquire)
    }

    pub fn result(&self) -> Result<(), String> {
        self.receipt.borrow().error.clone().map_or(Ok(()), Err)
    }

    pub async fn flush(&mut self) -> Result<(), String> {
        while self.is_waiting() {
            if self.receipt.changed().await.is_err() {
                break;
            }
        }
        self.result()
    }

    fn notice(&self) -> Option<&'static str> {
        if !self.is_waiting()
            || self
                .queue
                .since
                .lock()
                .unwrap()
                .is_none_or(|since| since.elapsed() < Duration::from_millis(150))
        {
            return None;
        }
        Some(if self.receipt.borrow().state.is_none() {
            "Loading preferences"
        } else {
            "Saving preferences"
        })
    }

    #[cfg(test)]
    pub async fn ready(&mut self) {
        while self.receipt.borrow().state.is_none() {
            self.receipt.changed().await.unwrap();
        }
    }
}

async fn apply_patch(
    previous: Arc<PersistedUiState>,
    patch: Patch,
) -> Result<Arc<PersistedUiState>, String> {
    tokio::task::spawn_blocking(move || {
        let mut state = (*previous).clone();
        patch.apply(&mut state);
        Arc::new(state)
    })
    .await
    .map_err(|error| format!("UI preference preparation stopped: {error}"))
}

impl PreferenceBinding {
    pub fn needs_initial_load(&self) -> bool {
        !self.loaded
    }

    pub fn submit(&self, app: &crate::app::AppState) {
        let mut delta = Patch::default();
        if delta.record(&mut self.observed.lock().unwrap(), app) {
            if !self.loaded {
                self.initial_overlay.lock().unwrap().merge(delta.clone());
            }
            let mut pending = self.store.queue.pending.lock().unwrap();
            pending.merge(delta);
            let revision = self.store.queue.desired.fetch_add(1, Ordering::AcqRel) + 1;
            pending.revision = revision;
            let mut since = self.store.queue.since.lock().unwrap();
            since.get_or_insert_with(Instant::now);
            let _ = self.store.wake.send(());
        }
    }

    pub fn refresh(&mut self, app: &mut crate::app::AppState) {
        let receipt = self.store.receipt.borrow().clone();
        if !self.loaded
            && let Some(state) = receipt.state
        {
            let mut state = (*state).clone();
            std::mem::take(&mut *self.initial_overlay.lock().unwrap()).apply(&mut state);
            state.apply(app);
            *self.observed.lock().unwrap() = state;
            self.loaded = true;
            app.mark_visual_dirty();
        }
        if let Some(error) = receipt.error
            && self.reported_error != Some(receipt.attempted)
        {
            self.reported_error = Some(receipt.attempted);
            app.push_note(
                format!("UI preferences could not be saved: {error}"),
                crate::app::NoteLevel::Warn,
            );
        }
        if let Some(label) = self.store.notice() {
            app.status_notes.insert(
                "preferences".into(),
                format!("{} {label}", crate::loading::glyph(app.animation_frame)),
            );
        } else {
            if self.store.receipt.borrow().error.is_some() {
                app.status_notes
                    .insert("preferences".into(), "Preferences not saved".into());
            } else {
                app.status_notes.remove("preferences");
            }
        }
    }
}

pub(crate) async fn preference_change(binding: &mut Option<PreferenceBinding>) {
    match binding {
        Some(binding) => {
            if binding.store.receipt.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound_app(store: &PreferenceStore) -> crate::app::AppState {
        let mut app = crate::app::AppState::new("preferences".into(), None);
        app.preferences = Some(store.bind(&mut app));
        app
    }

    #[tokio::test]
    async fn initial_read_and_stale_views_preserve_explicit_fields_and_panel_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("states.json");
        let saved = PersistedUiState {
            theme: atman_runtime::trust::Theme::Weather,
            goal_collapsed: true,
            panel_sizes: [("saved".into(), (80, 24))].into(),
            ..Default::default()
        };
        saved.save_to(&path).unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let (started, entered) = tokio::sync::oneshot::channel();
        let source = path.clone();
        let mut store = PreferenceStore::start_with_path(move || {
            let _ = started.send(());
            gate.recv().unwrap();
            Some(source)
        });
        entered.await.unwrap();
        let mut a = bound_app(&store);
        let mut b = bound_app(&store);
        a.goal_collapsed = true;
        a.save_ui_state();
        a.goal_collapsed = false;
        a.panel_sizes.insert("a".into(), (90, 25));
        a.save_ui_state();
        b.plan_collapsed = true;
        b.panel_sizes.insert("b".into(), (95, 26));
        b.save_ui_state();
        assert!(store.is_waiting());
        release.send(()).unwrap();
        store.ready().await;
        // This edit races the initial receipt, before the view has applied it.
        a.todo_collapsed = true;
        a.save_ui_state();
        a.refresh_preferences();
        assert!(a.todo_collapsed);
        assert!(!a.goal_collapsed);
        assert_eq!(a.trust.theme, atman_runtime::trust::Theme::Weather);
        store.flush().await.unwrap();
        b.refresh_preferences();
        b.context_collapsed = true;
        b.save_ui_state();
        store.flush().await.unwrap();
        let disk = PersistedUiState::load_from(&path);
        assert!(!disk.goal_collapsed);
        assert!(disk.plan_collapsed && disk.todo_collapsed && disk.context_collapsed);
        assert_eq!(disk.theme, saved.theme);
        assert_eq!(disk.panel_sizes.len(), 3);
        assert_eq!(disk.panel_sizes["saved"], (80, 24));
        assert_eq!(disk.panel_sizes["a"], (90, 25));
        assert_eq!(disk.panel_sizes["b"], (95, 26));
    }

    #[tokio::test]
    async fn failed_writes_settle_truthfully_and_later_edits_retry_retained_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("states.json");
        std::fs::create_dir(&path).unwrap();
        let destination = path.clone();
        let mut store = PreferenceStore::start_with_path(move || Some(destination));
        store.ready().await;
        let mut app = bound_app(&store);
        app.plan_collapsed = true;
        app.save_ui_state();
        assert!(store.flush().await.is_err());
        assert!(!store.is_waiting());
        app.refresh_preferences();
        assert_eq!(app.status_notes["preferences"], "Preferences not saved");
        std::fs::remove_dir(&path).unwrap();
        app.goal_collapsed = true;
        app.save_ui_state();
        store.flush().await.unwrap();
        let saved = PersistedUiState::load_from(&path);
        assert!(saved.goal_collapsed && saved.plan_collapsed);
        assert!(!store.is_waiting());
        app.refresh_preferences();
        assert!(!app.status_notes.contains_key("preferences"));
    }

    #[test]
    fn preference_patch_covers_the_persisted_scalar_schema() {
        let mut fields = Vec::new();
        let mut bits = std::collections::HashSet::new();
        macro_rules! names { ($($name:ident:$bit:literal),*) => { $(fields.push(stringify!($name)); assert!(bits.insert($bit));)* }; }
        fields!(names);
        fields.push("panel_sizes");
        fields.sort_unstable();
        let mut persisted = serde_json::to_value(PersistedUiState::default())
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        persisted.sort_unstable();
        assert_eq!(fields, persisted);
    }
}
