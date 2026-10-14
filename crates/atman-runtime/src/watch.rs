use crate::pending_batch::{Batch, PendingBatch};
use crate::value::AtmanPayload;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{StreamExt, stream::BoxStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::message::Message;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub type WatcherId = String;

#[derive(Debug, Clone)]
pub enum WatchSource {
    Terminal { handle: String },
    Bash { handle: String },
    Agent { handle: String },
}

impl WatchSource {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Terminal { .. } => "terminal",
            Self::Bash { .. } => "bash",
            Self::Agent { .. } => "agent",
        }
    }

    pub fn handle(&self) -> &str {
        match self {
            Self::Terminal { handle } | Self::Bash { handle } | Self::Agent { handle } => handle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    Once,
    Persist,
}

#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub watcher_id: WatcherId,
    pub source: WatchSource,
    pub pattern: String,
    pub row: Option<u16>,
    pub col: Option<u16>,
    pub text: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub timed_out: bool,
    pub exited: bool,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub enum WatchResult {
    Matched {
        row: Option<u16>,
        col: Option<u16>,
        text: String,
    },
    SourceExited,
    Cancelled,
}

struct Watcher {
    id: WatcherId,
    source: WatchSource,
    pattern: String,
    mode: WatchMode,
    timeout: Duration,
    created_at: chrono::DateTime<chrono::Utc>,
    cancel: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct WatcherInfo {
    pub id: WatcherId,
    pub source: WatchSource,
    pub pattern: String,
    pub mode: WatchMode,
    pub age: Duration,
    pub timeout: Duration,
}

pub struct WatchHub {
    state: Mutex<WatchState>,
    changes: watch::Sender<u64>,
}

#[derive(Default)]
struct WatchState {
    watchers: HashMap<WatcherId, Watcher>,
    pending_events: PendingBatch<WatchEvent>,
}

impl Default for WatchHub {
    fn default() -> Self {
        Self::new()
    }
}

impl WatchHub {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(WatchState::default()),
            changes: watch::channel(0).0,
        }
    }

    pub fn register(
        &self,
        source: WatchSource,
        pattern: String,
        mode: WatchMode,
        timeout: Duration,
    ) -> WatcherId {
        let id = format!("w_{}", uuid::Uuid::now_v7().simple());
        let cancel = CancellationToken::new();
        let watcher = Watcher {
            id: id.clone(),
            source,
            pattern,
            mode,
            timeout,
            created_at: chrono::Utc::now(),
            cancel,
        };
        self.state
            .lock()
            .unwrap()
            .watchers
            .insert(id.clone(), watcher);
        self.changed();
        id
    }

    pub fn unregister(&self, id: &WatcherId) -> bool {
        let removed = self.state.lock().unwrap().watchers.remove(id);
        if let Some(w) = removed {
            w.cancel.cancel();
            self.changed();
            true
        } else {
            false
        }
    }

    pub fn has_active_watchers(&self) -> bool {
        !self.state.lock().unwrap().watchers.is_empty()
    }

    /// Include queued events from watches that have already completed.
    pub fn has_work(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.pending_events.is_empty() || !state.watchers.is_empty()
    }

    pub fn has_pending_events(&self) -> bool {
        !self.state.lock().unwrap().pending_events.is_empty()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub fn list_active(&self) -> Vec<WatcherInfo> {
        let now = chrono::Utc::now();
        self.state
            .lock()
            .unwrap()
            .watchers
            .values()
            .map(|w| WatcherInfo {
                id: w.id.clone(),
                source: w.source.clone(),
                pattern: w.pattern.clone(),
                mode: w.mode,
                age: (now - w.created_at).to_std().unwrap_or(Duration::ZERO),
                timeout: w.timeout,
            })
            .collect()
    }

    pub fn list_watchers_for_handle(&self, handle: &str) -> Vec<WatcherInfo> {
        self.list_active()
            .into_iter()
            .filter(|w| w.source.handle() == handle)
            .collect()
    }

    pub fn get_cancel(&self, id: &WatcherId) -> CancellationToken {
        self.state
            .lock()
            .unwrap()
            .watchers
            .get(id)
            .map(|w| w.cancel.clone())
            .unwrap_or_else(|| {
                let cancel = CancellationToken::new();
                cancel.cancel();
                cancel
            })
    }

    pub fn enqueue_event(&self, event: WatchEvent) {
        let mut state = self.state.lock().unwrap();
        let Some(watcher) = state.watchers.get(&event.watcher_id) else {
            return; // Unwatch won the race against a late producer.
        };
        let removed = if watcher.mode == WatchMode::Once || event.timed_out || event.exited {
            state.watchers.remove(&event.watcher_id)
        } else {
            None
        };
        state.pending_events.push_back(event);
        drop(state);
        if let Some(watcher) = removed {
            watcher.cancel.cancel();
        }
        self.changed();
    }

    /// Wait without consuming events so a competing input or pause cannot lose them.
    pub async fn wait_for_pending(&self) {
        self.wait_until_ready(false).await;
    }

    /// A batch host may exit when all watches have been removed without an event.
    pub async fn wait_for_pending_or_idle(&self) {
        self.wait_until_ready(true).await;
    }

    async fn wait_until_ready(&self, include_idle: bool) {
        let mut changes = self.changes.subscribe();
        loop {
            let ready = {
                let state = self.state.lock().unwrap();
                !state.pending_events.is_empty() || (include_idle && state.watchers.is_empty())
            };
            if ready {
                return;
            }
            let _ = changes.changed().await;
        }
    }

    /// Reserve the FIFO batch without consuming it until history accepts it.
    pub fn reserve(self: &Arc<Self>) -> Option<WatchReceipt> {
        let batch = self.state.lock().unwrap().pending_events.reserve()?;
        Some(WatchReceipt {
            hub: self.clone(),
            batch,
        })
    }

    /// Deliver the current notification batch in arrival order, including retired watchers.
    pub fn take_pending_prompt(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        let batch = state.pending_events.reserve()?;
        let prompt = format_watch_batch(&batch.items);
        state
            .pending_events
            .commit(&batch, || Ok::<_, std::convert::Infallible>(()))
            .unwrap();
        drop(state);
        self.changed();
        Some(prompt)
    }

    pub async fn wait_for_event(&self, timeout: Duration) -> Option<WatchEvent> {
        tokio::time::timeout(timeout, async {
            let mut changes = self.changes.subscribe();
            loop {
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some(event);
                    }
                    if state.watchers.is_empty() && state.pending_events.is_empty() {
                        return None;
                    }
                }
                let _ = changes.changed().await;
            }
        })
        .await
        .ok()
        .flatten()
    }
}

/// Exclusive batch ownership shared by model pickup, background turns, and explicit waits.
pub struct WatchReceipt {
    hub: Arc<WatchHub>,
    batch: Batch<WatchEvent>,
}

impl WatchReceipt {
    pub fn prompt(&self) -> String {
        format_watch_batch(&self.batch.items)
    }

    /// The callback must not reenter this hub and must publish synchronously.
    pub fn commit<E>(&self, publish: impl FnOnce() -> Result<(), E>) -> Result<bool, E> {
        let committed = self
            .hub
            .state
            .lock()
            .unwrap()
            .pending_events
            .commit(&self.batch, publish)?;
        self.hub.changed();
        Ok(committed)
    }
}

impl Drop for WatchReceipt {
    fn drop(&mut self) {
        if self
            .hub
            .state
            .lock()
            .unwrap()
            .pending_events
            .release(&self.batch)
        {
            self.hub.changed();
        }
    }
}

fn format_watch_batch(events: &[WatchEvent]) -> String {
    format!(
        "Watcher notifications, not a new user request. Process these events using the recorded context; do not repeat completed work.\n\n{}",
        events
            .iter()
            .map(format_watch_event_text)
            .collect::<Vec<_>>()
            .join("\n\n")
    )
}

pub trait Watchable: Send + Sync {
    /// Establish subscriptions before returning. Sources with replaceable state must also capture their initial snapshot here. Keep the subscription and consumed output alive for the entire watch.
    fn watch_output(self: Arc<Self>, pattern: String) -> BoxStream<'static, WatchResult>;
}

/// Absolute byte position, including overlap for patterns split across updates.
#[derive(Default)]
pub(crate) struct OutputCursor(usize);

impl OutputCursor {
    pub(crate) fn find(&mut self, bytes: &[u8], base: usize, pattern: &str) -> Option<usize> {
        if pattern.is_empty() {
            return None;
        }
        let start = self.0.saturating_sub(base).min(bytes.len());
        if let Some(relative) = bytes[start..]
            .windows(pattern.len())
            .position(|window| window == pattern.as_bytes())
        {
            let position = base + start + relative;
            self.0 = position + pattern.len();
            Some(position)
        } else {
            self.0 = (base + start).max(base + bytes.len().saturating_sub(pattern.len() - 1));
            None
        }
    }
}

pub async fn handle_watch_result(
    hub: Arc<WatchHub>,
    wid: WatcherId,
    source: WatchSource,
    pattern: String,
    timeout: Duration,
    result: Result<WatchResult, tokio::time::error::Elapsed>,
) {
    match result {
        Ok(WatchResult::Matched { row, col, text }) => {
            hub.enqueue_event(WatchEvent {
                watcher_id: wid,
                source,
                pattern,
                row,
                col,
                text,
                timestamp: chrono::Utc::now(),
                timed_out: false,
                exited: false,
                timeout,
            });
        }
        Ok(WatchResult::SourceExited) => {
            hub.enqueue_event(WatchEvent {
                watcher_id: wid.clone(),
                source,
                pattern,
                row: None,
                col: None,
                text: String::new(),
                timestamp: chrono::Utc::now(),
                timed_out: false,
                exited: true,
                timeout,
            });
            hub.unregister(&wid);
        }
        Ok(WatchResult::Cancelled) => {
            hub.unregister(&wid);
        }
        Err(_) => {
            hub.enqueue_event(WatchEvent {
                watcher_id: wid,
                source,
                pattern,
                row: None,
                col: None,
                text: String::new(),
                timestamp: chrono::Utc::now(),
                timed_out: true,
                exited: false,
                timeout,
            });
        }
    }
}

pub fn format_watch_event_text(evt: &WatchEvent) -> String {
    let kind = evt.source.kind_str();
    let handle = evt.source.handle();
    if evt.exited {
        format!(
            "[watcher {}] {} '{}' has already exited. Pattern '{}' will never match.",
            evt.watcher_id, kind, handle, evt.pattern
        )
    } else if evt.timed_out {
        format!(
            "[watcher {}] {} '{}' pattern '{}' not detected in {}s. \
             Consider using {}.capture or {}.output to check current state.",
            evt.watcher_id,
            kind,
            handle,
            evt.pattern,
            evt.timeout.as_secs(),
            kind,
            kind
        )
    } else {
        format!(
            "[watcher {}] {} '{}' matched '{}' at row {:?}, col {:?}: {}",
            evt.watcher_id, kind, handle, evt.pattern, evt.row, evt.col, evt.text
        )
    }
}

fn watch_source_to_value(src: &WatchSource) -> Value {
    Value::Struct(vec![
        ("kind".into(), Value::Str(src.kind_str().into())),
        ("handle".into(), Value::Str(src.handle().into())),
    ])
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_optional_string(args: &ToolArgs, name: &str) -> Option<String> {
    args.named(name).and_then(|v| {
        if let Value::Str(s) = v {
            Some(s.clone())
        } else {
            None
        }
    })
}

fn extract_optional_int(args: &ToolArgs, name: &str) -> Option<i64> {
    args.named(name).and_then(|v| {
        if let Value::Int(i) = v {
            Some(*i)
        } else {
            None
        }
    })
}

pub struct Watch;
impl Tool for Watch {
    fn name(&self) -> &str {
        "watch"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Register a background watcher on any running task (terminal, bash, or agent).\n\
             When pattern appears in the task's output, the agent is woken up.\n\n\
             Non-blocking: returns watcher_id immediately.\n\
             mode: \"once\" (default) or \"persist\".\n\
             timeout_ms: default 120000 (120s), max 600000 (10min).",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "pattern": {"type": "string", "minLength": 1},
                "mode": {"type": "string", "enum": ["once", "persist"], "default": "once"},
                "timeout_ms": {"type": "integer", "default": 120000}
            },
            "required": ["handle", "pattern"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let pattern = extract_string(&args, "pattern", 1)?;
            if pattern.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "watch: pattern must not be empty".into(),
                ));
            }
            let mode = match extract_optional_string(&args, "mode").as_deref() {
                Some("persist") => WatchMode::Persist,
                _ => WatchMode::Once,
            };
            let timeout_ms = extract_optional_int(&args, "timeout_ms")
                .unwrap_or(120_000)
                .clamp(1_000, 600_000) as u64;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());

            let task_registry = ctx.task_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("watch: task registry not available".into())
            })?;
            let snapshot = task_registry.lookup_by_handle(&handle).ok_or_else(|| {
                RuntimeError::ToolFailed(format!("watch: handle '{handle}' not found"))
            })?;

            let watch_hub = ctx
                .watch_hub
                .clone()
                .ok_or_else(|| RuntimeError::ToolFailed("watch: watch hub not available".into()))?;

            let source = match snapshot.kind {
                crate::task_registry::TaskKind::Terminal => WatchSource::Terminal {
                    handle: handle.clone(),
                },
                crate::task_registry::TaskKind::Bash => WatchSource::Bash {
                    handle: handle.clone(),
                },
                crate::task_registry::TaskKind::Flow => WatchSource::Agent {
                    handle: handle.clone(),
                },
            };

            let watchable: Arc<dyn Watchable> = match snapshot.kind {
                crate::task_registry::TaskKind::Terminal => {
                    let reg = ctx.term_registry.clone().ok_or_else(|| {
                        RuntimeError::ToolFailed("watch: terminal registry not available".into())
                    })?;
                    reg.lookup(&handle, &session_id)?
                }
                crate::task_registry::TaskKind::Bash => {
                    let reg = ctx.bg_registry.clone().ok_or_else(|| {
                        RuntimeError::ToolFailed("watch: bash registry not available".into())
                    })?;
                    reg.lookup(&handle, &session_id)?
                }
                crate::task_registry::TaskKind::Flow => {
                    let reg = ctx.flow_registry.clone().ok_or_else(|| {
                        RuntimeError::ToolFailed("watch: agent registry not available".into())
                    })?;
                    reg.lookup(&handle)?
                }
            };
            let timeout = Duration::from_millis(timeout_ms);
            let watcher_id = watch_hub.register(source.clone(), pattern.clone(), mode, timeout);
            spawn_watcher(
                watch_hub,
                watcher_id.clone(),
                source,
                pattern,
                mode,
                timeout,
                watchable,
            );

            Ok(Value::Struct(vec![(
                "watcher_id".into(),
                Value::Str(watcher_id),
            )]))
        })
    }
}

fn spawn_watcher(
    hub: Arc<WatchHub>,
    wid: WatcherId,
    source: WatchSource,
    pattern: String,
    mode: WatchMode,
    timeout: Duration,
    watchable: Arc<dyn Watchable>,
) {
    let cancel = hub.get_cancel(&wid);
    let mut output = watchable.watch_output(pattern.clone());
    tokio::spawn(async move {
        loop {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                result = tokio::time::timeout(timeout, output.next()) => {
                    result.map(|item| item.unwrap_or(WatchResult::SourceExited))
                }
            };
            let matched = matches!(result, Ok(WatchResult::Matched { .. }));
            handle_watch_result(
                Arc::clone(&hub),
                wid.clone(),
                source.clone(),
                pattern.clone(),
                timeout,
                result,
            )
            .await;
            if !matched || mode == WatchMode::Once {
                break;
            }
            // A buffered source can produce many immediately-ready matches.
            tokio::task::yield_now().await;
        }
        hub.unregister(&wid);
    });
}

pub struct WatcherList;
impl Tool for WatcherList {
    fn name(&self) -> &str {
        "watcher.list"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("List all active background watchers with their source, pattern, mode, and age.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn call<'a>(&'a self, _args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let hub = ctx.watch_hub.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("watcher.list: watch hub not available".into())
            })?;
            let list = hub.list_active();
            let items: Vec<Value> = list
                .iter()
                .map(|w| {
                    Value::Struct(vec![
                        ("watcher_id".into(), Value::Str(w.id.clone())),
                        ("source".into(), watch_source_to_value(&w.source)),
                        ("pattern".into(), Value::Str(w.pattern.clone())),
                        (
                            "mode".into(),
                            Value::Str(
                                match w.mode {
                                    WatchMode::Once => "once",
                                    WatchMode::Persist => "persist",
                                }
                                .into(),
                            ),
                        ),
                        ("age_ms".into(), Value::Int(w.age.as_millis() as i64)),
                        (
                            "timeout_ms".into(),
                            Value::Int(w.timeout.as_millis() as i64),
                        ),
                    ])
                })
                .collect();
            Ok(Value::List(items))
        })
    }
}

pub struct WatcherUnwatch;
impl Tool for WatcherUnwatch {
    fn name(&self) -> &str {
        "watcher.unwatch"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Cancel a background watcher by id. Works for any source (terminal/bash/agent).")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "watcher_id": {"type": "string"}
            },
            "required": ["watcher_id"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let id = extract_string(&args, "watcher_id", 0)?;
            let hub = ctx.watch_hub.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("watcher.unwatch: watch hub not available".into())
            })?;
            if hub.unregister(&id) {
                Ok(Value::Unit)
            } else {
                Err(RuntimeError::ToolFailed(format!(
                    "watcher.unwatch: '{id}' not found"
                )))
            }
        })
    }
}

pub struct WaitForWatcher;
impl Tool for WaitForWatcher {
    fn name(&self) -> &str {
        "wait_for_watcher"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Block until a registered watcher fires, or timeout.\n\
             If no active watchers exist, returns immediately (Unit).\n\
             Returns Str with event details on match, or Unit on timeout/no watchers.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timeout_ms": {"type": "integer", "default": 30000}
            }
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let timeout_ms = extract_optional_int(&args, "timeout_ms")
                .unwrap_or(30_000)
                .clamp(100, 300_000) as u64;
            let hub = ctx.watch_hub.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("wait_for_watcher: watch hub not available".into())
            })?;
            match hub.wait_for_event(Duration::from_millis(timeout_ms)).await {
                Some(evt) => Ok(Value::Host(AtmanPayload::Message(Message::system_text(
                    ctx.turn_id
                        .clone()
                        .unwrap_or_else(crate::event::TurnId::now),
                    format_watch_event_text(&evt),
                )))),
                None => Ok(Value::Unit),
            }
        })
    }
}

pub struct HasPendingInjections;
impl Tool for HasPendingInjections {
    fn name(&self) -> &str {
        "has_pending_injections"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Check if there are pending user injections for the current turn.\n\
             Returns true if any exist — the agent loop should continue so they get drained.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn call<'a>(&'a self, _args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            if ctx
                .agent_entry
                .as_ref()
                .is_some_and(|entry| !entry.pending_injections.lock().unwrap().is_empty())
            {
                return Ok(Value::Bool(true));
            }
            if matches!(ctx.history_segment, crate::tool::HistorySegment::Spawned)
                && ctx.agent_entry.is_none()
            {
                return Ok(Value::Bool(false));
            }
            let Some(session) = ctx.session_runtime.as_ref() else {
                return if ctx.agent_entry.is_some() {
                    Ok(Value::Bool(false))
                } else {
                    Err(RuntimeError::ToolFailed(
                        "has_pending_injections: no current flow".into(),
                    ))
                };
            };
            let turn_id = ctx.turn_id.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("has_pending_injections: no turn id".into())
            })?;
            let has_pending = session.list_pending_injections().iter().any(|inj| {
                inj.turn_id == turn_id && inj.state == crate::injection::InjectionState::Pending
            });
            Ok(Value::Bool(has_pending))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reserved_watch_batch_excludes_other_consumers_and_rolls_back_without_loss() {
        let hub = Arc::new(WatchHub::new());
        let source = WatchSource::Bash {
            handle: "job".into(),
        };
        let id = hub.register(
            source.clone(),
            "match".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        hub.enqueue_event(WatchEvent {
            watcher_id: id,
            source,
            pattern: "match".into(),
            row: None,
            col: None,
            text: "first".into(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: false,
            timeout: Duration::from_secs(10),
        });
        let receipt = hub.reserve().unwrap();
        assert!(hub.reserve().is_none());
        assert!(hub.take_pending_prompt().is_none());
        assert!(hub.wait_for_event(Duration::from_millis(1)).await.is_none());
        assert_eq!(
            receipt.commit(|| Err("history failed")),
            Err("history failed")
        );
        assert!(hub.has_pending_events());
        drop(receipt);
        let event = hub.wait_for_event(Duration::from_secs(1)).await.unwrap();
        assert_eq!(event.text, "first");
        assert!(!hub.has_pending_events());
        assert!(hub.reserve().is_none());
    }

    #[test]
    fn output_cursor_consumes_matches_and_preserves_split_patterns() {
        let mut cursor = OutputCursor::default();
        assert_eq!(cursor.find("完".as_bytes(), 0, "完成"), None);
        assert_eq!(cursor.find("完成完成".as_bytes(), 0, "完成"), Some(0));
        assert_eq!(cursor.find("完成完成".as_bytes(), 0, "完成"), Some(6));
        assert_eq!(cursor.find("完成完成".as_bytes(), 0, "完成"), None);
        // A ring buffer can discard bytes before the consumed position.
        assert_eq!(cursor.find(b"xxdone", 100, "done"), Some(102));
        assert_eq!(cursor.find(b"done", 102, "done"), None);
        assert_eq!(cursor.find(b"done", 102, ""), None);
    }

    #[tokio::test]
    async fn flow_watch_payload_depends_on_matches_not_output_history() {
        use crate::tools::agent_ctrl::{FlowRegistry, FlowRunStatus};
        let entry = FlowRegistry::new().create_entry(
            "watch-flow".into(),
            String::new(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        let pattern = "完成";
        let history = format!("{pattern}{}", "x".repeat(58)).repeat(128);
        entry.append_output(None, &history);
        entry.finish(FlowRunStatus::Ok {
            ended_at: chrono::Utc::now(),
            final_text: String::new(),
        });
        let mut stream = Arc::clone(&entry).watch_output(pattern.into());
        let mut matches = 0;
        while let Some(result) = stream.next().await {
            if let WatchResult::Matched { text, .. } = result {
                assert_eq!(text, pattern);
                matches += 1;
            }
        }
        assert_eq!(matches, 128);
        // Tool-only final responses use the same bounded payload contract.
        let entry = FlowRegistry::new().create_entry(
            "final-only".into(),
            String::new(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        entry.finish(FlowRunStatus::Ok {
            ended_at: chrono::Utc::now(),
            final_text: format!("{}{pattern}", "x".repeat(8192)),
        });
        let mut stream = entry.watch_output(pattern.into());
        assert!(
            matches!(stream.next().await, Some(WatchResult::Matched { text, .. }) if text == pattern)
        );
        assert!(matches!(
            stream.next().await,
            Some(WatchResult::SourceExited)
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn flow_watch_consumes_history_once_and_keeps_its_subscription() {
        use crate::tools::agent_ctrl::{FlowRegistry, FlowRunStatus};
        use futures::FutureExt;
        let entry = FlowRegistry::new().create_entry(
            "watch-flow".into(),
            String::new(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        entry.append_output(None, "done");
        let mut output = Arc::clone(&entry).watch_output("done".into());
        assert!(matches!(
            output.next().await,
            Some(WatchResult::Matched { .. })
        ));
        assert!(output.next().now_or_never().is_none());
        // Output arrives between polls, while the subscription stays alive.
        entry.append_output(None, " done");
        entry.complete_output();
        assert!(matches!(
            output.next().await,
            Some(WatchResult::Matched { .. })
        ));
        assert!(output.next().now_or_never().is_none());
        let status = FlowRunStatus::Ok {
            ended_at: chrono::Utc::now(),
            final_text: "done".into(),
        };
        entry.finish(status);
        assert!(matches!(
            output.next().await,
            Some(WatchResult::SourceExited)
        ));
        assert!(output.next().await.is_none());

        // A completed flow with only a final tool response also terminates.
        let entry = FlowRegistry::new().create_entry(
            "final-only".into(),
            String::new(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        entry.finish(FlowRunStatus::Ok {
            ended_at: chrono::Utc::now(),
            final_text: "done".into(),
        });
        let mut output = entry.watch_output("done".into());
        assert!(matches!(
            output.next().await,
            Some(WatchResult::Matched { .. })
        ));
        assert!(matches!(
            output.next().await,
            Some(WatchResult::SourceExited)
        ));
        assert!(output.next().await.is_none());
    }

    #[tokio::test]
    async fn flow_final_delivery_preserves_cursors_across_turns_and_lag() {
        use crate::event::{FlowRunId, TurnId};
        use crate::tools::agent_ctrl::{FlowRegistry, FlowRunStatus};
        use futures::FutureExt;
        let entry = FlowRegistry::new().create_entry(
            "job".into(),
            String::new(),
            String::new(),
            FlowRunId::now(),
        );
        let first = TurnId::now();
        entry.append_output(Some(&first), "long preliminary output ");
        let mut output = Arc::clone(&entry).watch_output("完成".into());
        assert!(output.next().now_or_never().is_none());
        entry.publish_final_answer(&first, "完成");
        assert!(matches!(
            output.next().await,
            Some(WatchResult::Matched { .. })
        ));
        entry.publish_final_answer(&first, "完成");
        assert!(output.next().now_or_never().is_none());

        // Delayed polling must retain every delivery, including identical answers
        // in later turns and enough notifications to overflow the broadcast queue.
        for _ in 0..128 {
            entry.publish_final_answer(&TurnId::now(), "完成");
        }
        for _ in 0..128 {
            assert!(matches!(
                output.next().await,
                Some(WatchResult::Matched { .. })
            ));
        }
        assert!(output.next().now_or_never().is_none());
        let streamed = TurnId::now();
        entry.append_output(Some(&streamed), "完");
        entry.complete_output();
        assert!(output.next().now_or_never().is_none());
        entry.append_output(Some(&streamed), "成");
        entry.complete_output();
        assert!(matches!(
            output.next().await,
            Some(WatchResult::Matched { .. })
        ));
        entry.publish_final_answer(&streamed, "完成");
        assert!(output.next().now_or_never().is_none());
        entry.finish(FlowRunStatus::Ok {
            ended_at: chrono::Utc::now(),
            final_text: "完成".into(),
        });
        assert!(matches!(
            output.next().await,
            Some(WatchResult::SourceExited)
        ));
        assert!(output.next().await.is_none());
        assert_eq!(entry.output().matches("完成").count(), 130);
    }

    #[tokio::test]
    async fn removing_the_last_watch_wakes_all_waiters_and_rejects_late_results() {
        let hub = Arc::new(WatchHub::new());
        let source = WatchSource::Bash {
            handle: "job".into(),
        };
        let id = hub.register(
            source.clone(),
            "done".into(),
            WatchMode::Once,
            Duration::from_secs(1),
        );
        let first = hub.wait_for_pending_or_idle();
        let second = hub.wait_for_pending_or_idle();
        tokio::pin!(first, second);
        for waiter in [&mut first, &mut second] {
            tokio::select! {
                biased;
                _ = waiter => panic!("watch is still active"),
                _ = std::future::ready(()) => {},
            }
        }
        hub.unregister(&id);
        assert!(hub.get_cancel(&id).is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(first, second);
        })
        .await
        .unwrap();
        handle_watch_result(
            hub.clone(),
            id,
            source,
            "done".into(),
            Duration::from_secs(1),
            Ok(WatchResult::Matched {
                row: None,
                col: None,
                text: "late".into(),
            }),
        )
        .await;
        assert!(!hub.has_work());
    }

    #[tokio::test]
    async fn watch_registration_subscribes_before_acknowledging_success() {
        use crate::tools::agent_ctrl::FlowRegistry;
        let flows = Arc::new(FlowRegistry::new());
        let entry = flows.create_entry(
            "job".into(),
            String::new(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        let tasks = crate::task_registry::TaskRegistry::new();
        tasks.register(
            crate::task_registry::TaskKind::Flow,
            crate::task_registry::TaskDisplay {
                label: "job".into(),
                command: None,
            },
            "job".into(),
            "session".into(),
            CancellationToken::new(),
        );
        let hub = Arc::new(WatchHub::new());
        let ctx = ToolCtx::new()
            .with_session_id("session")
            .with_task_registry(tasks)
            .with_flow_registry(flows)
            .with_watch_hub(hub.clone());
        let result = Watch
            .call(
                ToolArgs {
                    positional: vec![Value::Str("job".into()), Value::Str("done".into())],
                    named: vec![],
                },
                &ctx,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(entry.stream_tx.receiver_count(), 1);
        let active = hub.list_active();
        assert_eq!(active.len(), 1);
        // Cancellation before the producer first polls still releases its subscription.
        hub.unregister(&active[0].id);
        tokio::task::yield_now().await;
        assert_eq!(entry.stream_tx.receiver_count(), 0);
        assert!(!hub.has_work());
    }

    #[tokio::test]
    async fn failed_watch_target_resolution_leaves_no_active_registration() {
        let hub = Arc::new(WatchHub::new());
        let tasks = crate::task_registry::TaskRegistry::new();
        tasks.register(
            crate::task_registry::TaskKind::Bash,
            crate::task_registry::TaskDisplay {
                label: "job".into(),
                command: None,
            },
            "job".into(),
            "session".into(),
            CancellationToken::new(),
        );
        let mut ctx = ToolCtx::new()
            .with_session_id("session")
            .with_task_registry(tasks);
        ctx.watch_hub = Some(hub.clone());
        let result = Watch
            .call(
                ToolArgs {
                    positional: vec![Value::Str("job".into()), Value::Str("done".into())],
                    named: vec![],
                },
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(!hub.has_work());
    }

    #[tokio::test]
    async fn pending_injection_check_uses_the_current_flow_store() {
        let session = Arc::new(crate::session::Session::open_ephemeral());
        let turn_id = crate::event::TurnId::now();
        session.begin_turn(crate::message::Message::user_text(turn_id.clone(), "root"));
        session.enqueue_injection("root correction").unwrap();
        let root_ctx = ToolCtx::new()
            .with_anchors(Some(turn_id), None, None)
            .with_session_runtime(session);
        assert!(matches!(
            HasPendingInjections
                .call(ToolArgs::default(), &root_ctx)
                .await
                .unwrap(),
            Value::Bool(true)
        ));

        let registry = crate::tools::agent_ctrl::FlowRegistry::new();
        let entry = registry.create_entry(
            "child".into(),
            "goal".into(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        let child_ctx = ToolCtx::new().with_agent_entry(Arc::clone(&entry));
        assert!(matches!(
            HasPendingInjections
                .call(ToolArgs::default(), &child_ctx)
                .await
                .unwrap(),
            Value::Bool(false)
        ));
        entry
            .pending_injections
            .lock()
            .unwrap()
            .push(crate::injection::Injection::with_level(
                crate::event::TurnId::now(),
                "child correction",
                crate::injection::InjectionLevel::L1Nudge,
                None,
            ));
        assert!(matches!(
            HasPendingInjections
                .call(ToolArgs::default(), &child_ctx)
                .await
                .unwrap(),
            Value::Bool(true)
        ));
    }

    #[test]
    fn watch_hub_register_and_unregister() {
        let hub = WatchHub::new();
        let id = hub.register(
            WatchSource::Terminal {
                handle: "t1".into(),
            },
            "$ ".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        assert!(hub.has_active_watchers());
        assert!(hub.unregister(&id));
        assert!(!hub.has_active_watchers());
    }

    #[test]
    fn watch_hub_list_watchers_for_handle() {
        let hub = WatchHub::new();
        hub.register(
            WatchSource::Terminal {
                handle: "t1".into(),
            },
            "$ ".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        hub.register(
            WatchSource::Terminal {
                handle: "t1".into(),
            },
            "error".into(),
            WatchMode::Persist,
            Duration::from_secs(30),
        );
        hub.register(
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        let t1_watchers = hub.list_watchers_for_handle("t1");
        assert_eq!(t1_watchers.len(), 2);
        let b1_watchers = hub.list_watchers_for_handle("b1");
        assert_eq!(b1_watchers.len(), 1);
        assert!(hub.list_watchers_for_handle("nonexistent").is_empty());
    }

    #[test]
    fn watch_hub_enqueue_event_removes_once_mode() {
        let hub = WatchHub::new();
        let id = hub.register(
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        hub.enqueue_event(WatchEvent {
            watcher_id: id.clone(),
            source: WatchSource::Bash {
                handle: "b1".into(),
            },
            pattern: "done".into(),
            row: Some(0),
            col: Some(0),
            text: "done".into(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: false,
            timeout: Duration::from_secs(10),
        });
        assert!(
            !hub.has_active_watchers(),
            "once-mode watcher should be auto-removed after event"
        );
    }

    #[test]
    fn watch_hub_enqueue_event_keeps_persist_mode() {
        let hub = WatchHub::new();
        let id = hub.register(
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Persist,
            Duration::from_secs(10),
        );
        hub.enqueue_event(WatchEvent {
            watcher_id: id,
            source: WatchSource::Bash {
                handle: "b1".into(),
            },
            pattern: "done".into(),
            row: Some(0),
            col: Some(0),
            text: "done".into(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: false,
            timeout: Duration::from_secs(10),
        });
        assert!(
            hub.has_active_watchers(),
            "persist-mode watcher should remain after event"
        );
    }

    #[test]
    fn format_event_text_includes_watcher_id_and_pattern() {
        let evt = WatchEvent {
            watcher_id: "w_abc123".into(),
            source: WatchSource::Terminal {
                handle: "term_x".into(),
            },
            pattern: "$ ".into(),
            row: Some(10),
            col: Some(0),
            text: "$ ls".into(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: false,
            timeout: Duration::from_secs(120),
        };
        let text = format_watch_event_text(&evt);
        assert!(text.contains("w_abc123"));
        assert!(text.contains("terminal"));
        assert!(text.contains("term_x"));
        assert!(text.contains("$ "));
    }

    #[test]
    fn format_event_text_timeout_includes_suggestion() {
        let evt = WatchEvent {
            watcher_id: "w_abc".into(),
            source: WatchSource::Bash {
                handle: "b1".into(),
            },
            pattern: "done".into(),
            row: None,
            col: None,
            text: String::new(),
            timestamp: chrono::Utc::now(),
            timed_out: true,
            exited: false,
            timeout: Duration::from_secs(120),
        };
        let text = format_watch_event_text(&evt);
        assert!(text.contains("not detected"));
        assert!(text.contains("capture or"));
    }

    #[tokio::test]
    async fn wait_for_event_returns_none_when_no_watchers() {
        let hub = WatchHub::new();
        let result = hub.wait_for_event(Duration::from_millis(100)).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn wait_for_event_returns_pending_event() {
        let hub = Arc::new(WatchHub::new());
        let id = hub.register(
            WatchSource::Terminal {
                handle: "t1".into(),
            },
            "$ ".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        hub.enqueue_event(WatchEvent {
            watcher_id: id,
            source: WatchSource::Terminal {
                handle: "t1".into(),
            },
            pattern: "$ ".into(),
            row: Some(0),
            col: Some(0),
            text: "$ ".into(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: false,
            timeout: Duration::from_secs(10),
        });
        let evt = hub.wait_for_event(Duration::from_millis(100)).await;
        assert!(evt.is_some());
        assert_eq!(evt.unwrap().pattern, "$ ");
    }

    #[tokio::test]
    async fn wait_for_event_returns_source_exited_event() {
        // When a watcher's source exits, handle_watch_result enqueues a WatchEvent
        // with exited=true and unregisters the watcher. wait_for_event must still
        // return that event even though no watchers remain active.
        let hub = Arc::new(WatchHub::new());
        let wid = hub.register(
            WatchSource::Agent {
                handle: "a1".into(),
            },
            "done".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        // Simulate source-exited: enqueue event then unregister (mirrors handle_watch_result)
        hub.enqueue_event(WatchEvent {
            watcher_id: wid.clone(),
            source: WatchSource::Agent {
                handle: "a1".into(),
            },
            pattern: "done".into(),
            row: None,
            col: None,
            text: String::new(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: true,
            timeout: Duration::from_secs(10),
        });
        hub.unregister(&wid);
        assert!(!hub.has_active_watchers());
        // Must still return the pending exited event despite no active watchers
        let evt = hub.wait_for_event(Duration::from_millis(100)).await;
        assert!(
            evt.is_some(),
            "exited event must be consumable via wait_for_event"
        );
        let evt = evt.unwrap();
        assert!(evt.exited);
        assert_eq!(evt.pattern, "done");
    }

    #[test]
    fn format_event_text_exited_includes_already_exited() {
        let evt = WatchEvent {
            watcher_id: "w_xyz".into(),
            source: WatchSource::Agent {
                handle: "a1".into(),
            },
            pattern: "done".into(),
            row: None,
            col: None,
            text: String::new(),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: true,
            timeout: Duration::from_secs(120),
        };
        let text = format_watch_event_text(&evt);
        assert!(text.contains("already exited"));
        assert!(text.contains("a1"));
        assert!(text.contains("done"));
    }

    struct ScriptedWatchable {
        results: Vec<WatchResult>,
    }

    impl Watchable for ScriptedWatchable {
        fn watch_output(self: Arc<Self>, _pattern: String) -> BoxStream<'static, WatchResult> {
            Box::pin(futures::stream::iter(self.results.clone()))
        }
    }

    #[tokio::test]
    async fn persist_watcher_fires_on_every_match_then_cleans_up() {
        let hub = Arc::new(WatchHub::new());
        let wid = hub.register(
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Persist,
            Duration::from_secs(10),
        );
        let watchable: Arc<dyn Watchable> = Arc::new(ScriptedWatchable {
            results: vec![
                WatchResult::Matched {
                    row: None,
                    col: Some(0),
                    text: "done".into(),
                },
                WatchResult::Matched {
                    row: None,
                    col: Some(4),
                    text: "done".into(),
                },
                WatchResult::SourceExited,
            ],
        });
        spawn_watcher(
            Arc::clone(&hub),
            wid,
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Persist,
            Duration::from_secs(10),
            watchable,
        );

        let e1 = hub
            .wait_for_event(Duration::from_secs(2))
            .await
            .expect("first match event");
        assert!(!e1.exited && !e1.timed_out, "first event should be a match");
        let e2 = hub
            .wait_for_event(Duration::from_secs(2))
            .await
            .expect("second match event");
        assert!(
            !e2.exited && !e2.timed_out,
            "second event should be a match"
        );
        let e3 = hub
            .wait_for_event(Duration::from_secs(2))
            .await
            .expect("exited event");
        assert!(e3.exited, "third event should be source-exited");

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !hub.has_active_watchers(),
            "persist watcher must be removed after source exits"
        );
    }

    #[tokio::test]
    async fn persist_watcher_timeout_removes_watcher() {
        struct NeverMatch;
        impl Watchable for NeverMatch {
            fn watch_output(self: Arc<Self>, _pattern: String) -> BoxStream<'static, WatchResult> {
                Box::pin(futures::stream::pending())
            }
        }

        let hub = Arc::new(WatchHub::new());
        let wid = hub.register(
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Persist,
            Duration::from_millis(50),
        );
        spawn_watcher(
            Arc::clone(&hub),
            wid,
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Persist,
            Duration::from_millis(50),
            Arc::new(NeverMatch) as Arc<dyn Watchable>,
        );

        let evt = hub
            .wait_for_event(Duration::from_secs(2))
            .await
            .expect("timeout event");
        assert!(evt.timed_out, "should receive a timeout event");

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !hub.has_active_watchers(),
            "persist watcher must be removed after timeout, not linger forever"
        );
    }

    #[tokio::test]
    async fn persist_watcher_stops_on_cancel() {
        struct NeverMatch;
        impl Watchable for NeverMatch {
            fn watch_output(self: Arc<Self>, _pattern: String) -> BoxStream<'static, WatchResult> {
                Box::pin(futures::stream::pending())
            }
        }

        let hub = Arc::new(WatchHub::new());
        let wid = hub.register(
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Persist,
            Duration::from_secs(10),
        );
        spawn_watcher(
            Arc::clone(&hub),
            wid.clone(),
            WatchSource::Bash {
                handle: "b1".into(),
            },
            "done".into(),
            WatchMode::Persist,
            Duration::from_secs(10),
            Arc::new(NeverMatch) as Arc<dyn Watchable>,
        );

        hub.unregister(&wid);

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !hub.has_active_watchers(),
            "persist watcher must stop and be removed when cancelled"
        );
    }

    #[tokio::test]
    async fn persist_watcher_yields_and_cancels_an_always_ready_source() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Ready(Arc<AtomicUsize>);
        impl Watchable for Ready {
            fn watch_output(self: Arc<Self>, pattern: String) -> BoxStream<'static, WatchResult> {
                Box::pin(futures::stream::repeat_with(move || {
                    self.0.fetch_add(1, Ordering::Relaxed);
                    WatchResult::Matched {
                        row: None,
                        col: None,
                        text: pattern.clone(),
                    }
                }))
            }
        }
        let hub = Arc::new(WatchHub::new());
        let source = WatchSource::Bash {
            handle: "ready".into(),
        };
        let timeout = Duration::from_secs(1);
        let wid = hub.register(source.clone(), "done".into(), WatchMode::Persist, timeout);
        let count = Arc::new(AtomicUsize::new(0));
        spawn_watcher(
            Arc::clone(&hub),
            wid.clone(),
            source,
            "done".into(),
            WatchMode::Persist,
            timeout,
            Arc::new(Ready(Arc::clone(&count))),
        );
        assert!(hub.wait_for_event(timeout).await.is_some());
        hub.unregister(&wid);
        let observed = count.load(Ordering::Relaxed);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::Relaxed), observed);
    }
}
