use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
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
    watchers: Mutex<HashMap<WatcherId, Watcher>>,
    pending_events: Mutex<Vec<WatchEvent>>,
    notify: Notify,
}

impl Default for WatchHub {
    fn default() -> Self {
        Self::new()
    }
}

impl WatchHub {
    pub fn new() -> Self {
        Self {
            watchers: Mutex::new(HashMap::new()),
            pending_events: Mutex::new(Vec::new()),
            notify: Notify::new(),
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
        self.watchers.lock().unwrap().insert(id.clone(), watcher);
        id
    }

    pub fn unregister(&self, id: &WatcherId) -> bool {
        if let Some(w) = self.watchers.lock().unwrap().remove(id) {
            w.cancel.cancel();
            true
        } else {
            false
        }
    }

    pub fn has_active_watchers(&self) -> bool {
        !self.watchers.lock().unwrap().is_empty()
    }

    pub fn list_active(&self) -> Vec<WatcherInfo> {
        let now = chrono::Utc::now();
        self.watchers
            .lock()
            .unwrap()
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
        self.watchers
            .lock()
            .unwrap()
            .get(id)
            .map(|w| w.cancel.clone())
            .unwrap_or_default()
    }

    pub fn enqueue_event(&self, event: WatchEvent) {
        let should_remove = {
            let watchers = self.watchers.lock().unwrap();
            watchers
                .get(&event.watcher_id)
                .map(|w| matches!(w.mode, WatchMode::Once))
                .unwrap_or(false)
        };
        if should_remove {
            if let Some(w) = self.watchers.lock().unwrap().remove(&event.watcher_id) {
                w.cancel.cancel();
            }
        }
        self.pending_events.lock().unwrap().push(event);
        self.notify.notify_one();
    }

    pub async fn wait_for_event(&self, timeout: Duration) -> Option<WatchEvent> {
        if let Some(evt) = self.pending_events.lock().unwrap().pop() {
            return Some(evt);
        }
        if !self.has_active_watchers() {
            return None;
        }
        let _ = tokio::time::timeout(timeout, self.notify.notified()).await;
        self.pending_events.lock().unwrap().pop()
    }
}

pub trait Watchable: Send + Sync {
    fn watch_output(
        self: Arc<Self>,
        pattern: String,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WatchResult> + Send>>;
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
                "pattern": {"type": "string"},
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

            let watcher_id = watch_hub.register(
                source.clone(),
                pattern.clone(),
                mode,
                Duration::from_millis(timeout_ms),
            );

            match snapshot.kind {
                crate::task_registry::TaskKind::Terminal => {
                    let reg = ctx.term_registry.clone().ok_or_else(|| {
                        RuntimeError::ToolFailed("watch: terminal registry not available".into())
                    })?;
                    let entry = reg.lookup(&handle, &session_id)?;
                    let watchable: Arc<dyn Watchable> = Arc::clone(&entry) as Arc<dyn Watchable>;
                    spawn_watcher(
                        watch_hub,
                        watcher_id.clone(),
                        source,
                        pattern,
                        mode,
                        Duration::from_millis(timeout_ms),
                        watchable,
                    );
                }
                crate::task_registry::TaskKind::Bash => {
                    let reg = ctx.bg_registry.clone().ok_or_else(|| {
                        RuntimeError::ToolFailed("watch: bash registry not available".into())
                    })?;
                    let entry = reg.lookup(&handle, &session_id)?;
                    let watchable: Arc<dyn Watchable> = Arc::clone(&entry) as Arc<dyn Watchable>;
                    spawn_watcher(
                        watch_hub,
                        watcher_id.clone(),
                        source,
                        pattern,
                        mode,
                        Duration::from_millis(timeout_ms),
                        watchable,
                    );
                }
                crate::task_registry::TaskKind::Flow => {
                    let reg = ctx.flow_registry.clone().ok_or_else(|| {
                        RuntimeError::ToolFailed("watch: agent registry not available".into())
                    })?;
                    let entry = reg.lookup(&handle)?;
                    let watchable: Arc<dyn Watchable> = Arc::clone(&entry) as Arc<dyn Watchable>;
                    spawn_watcher(
                        watch_hub,
                        watcher_id.clone(),
                        source,
                        pattern,
                        mode,
                        Duration::from_millis(timeout_ms),
                        watchable,
                    );
                }
            }

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
    let w = Arc::clone(&watchable);
    let pat = pattern.clone();
    tokio::spawn(async move {
        loop {
            let result = tokio::time::timeout(
                timeout,
                Arc::clone(&w).watch_output(pat.clone(), cancel.clone()),
            )
            .await;
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
        }
        if mode == WatchMode::Persist {
            hub.unregister(&wid);
        }
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
                Some(evt) => Ok(Value::Message(Message::system_text(
                    ctx.turn_id
                        .clone()
                        .unwrap_or_else(crate::event::TurnId::now),
                    format_watch_event_text(&evt),
                ))),
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
            let session = ctx.session_runtime.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("has_pending_injections: no session".into())
            })?;
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
        hub.register(
            WatchSource::Terminal {
                handle: "t1".into(),
            },
            "$ ".into(),
            WatchMode::Once,
            Duration::from_secs(10),
        );
        hub.enqueue_event(WatchEvent {
            watcher_id: "w_test".into(),
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
        idx: Mutex<usize>,
    }

    impl Watchable for ScriptedWatchable {
        fn watch_output(
            self: Arc<Self>,
            _pattern: String,
            _cancel: CancellationToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WatchResult> + Send>> {
            let mut idx = self.idx.lock().unwrap();
            let i = *idx;
            *idx += 1;
            let result = self
                .results
                .get(i)
                .cloned()
                .unwrap_or(WatchResult::SourceExited);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                result
            })
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
            idx: Mutex::new(0),
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
            fn watch_output(
                self: Arc<Self>,
                _pattern: String,
                cancel: CancellationToken,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WatchResult> + Send>>
            {
                Box::pin(async move {
                    cancel.cancelled().await;
                    WatchResult::Cancelled
                })
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
            fn watch_output(
                self: Arc<Self>,
                _pattern: String,
                cancel: CancellationToken,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WatchResult> + Send>>
            {
                Box::pin(async move {
                    cancel.cancelled().await;
                    WatchResult::Cancelled
                })
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
}
