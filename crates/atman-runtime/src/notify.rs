//! Unified notification system for atman.
//!
//! ```ignore
//! use atman_runtime::notify::{self, NotifyLocation, NotifyStack};
//!
//! notify!(info, "copied {} chars to clipboard", n);
//! notify!(warn, location = Inline, stack = dedupe("key", 30_000), "index unavailable");
//! notify!(success, location = Toast, "todo marked done");
//! notify!(debug, target: "auto_snapshot", "{} @ {}", name, rev);
//! ```

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

// ── Level ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotifyLevel {
    Error,
    Warn,
    Info,
    Success,
    Debug,
}

impl NotifyLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Success => "SUCCESS",
            Self::Debug => "DEBUG",
        }
    }
}

// ── Location ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotifyLocation {
    /// Inline in the transcript / event stream; persists in scrollback.
    Inline,
    /// Top-right corner toast; auto-dismisses.
    Toast,
    /// Status bar single slot; overwrites on same key.
    Status,
    /// Modal dialog; requires user dismissal.
    Modal,
    /// Non-TUI stdout.
    Stdout,
    /// Non-TUI stderr.
    Stderr,
    /// Log only; never shown to the user.
    Log,
}

// ── Lifecycle ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyLifecycle {
    /// Persist in transcript / output items.
    Persistent,
    /// Auto-dismiss after a duration.
    Ttl(Duration),
    /// User can dismiss manually (Esc / click).
    Dismissible,
    /// Overwrite the previous notification with the same key.
    UntilReplaced,
}

// ── Stack strategy ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyStack {
    /// Always append a new entry.
    Append,
    /// Replace the previous notification with the same key.
    Replace { key: String },
    /// Suppress duplicates of the same key within a time window.
    Dedupe { key: String, window: Duration },
    /// Merge into a single entry with a count; useful for bursty errors.
    MergeCount { key: String, window: Duration },
    /// Keep only the latest message for a given key.
    Coalesce { key: String },
}

// ── Notification ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Notification {
    pub level: NotifyLevel,
    pub location: NotifyLocation,
    pub lifecycle: NotifyLifecycle,
    pub stack: NotifyStack,
    pub message: String,
    pub target: Option<String>,
    pub created_at: Instant,
}

impl Notification {
    pub fn new(level: NotifyLevel, message: impl Into<String>) -> Self {
        Self {
            level,
            location: NotifyLocation::Inline,
            lifecycle: NotifyLifecycle::Persistent,
            stack: NotifyStack::Append,
            message: message.into(),
            target: None,
            created_at: Instant::now(),
        }
    }

    pub fn with_location(mut self, location: NotifyLocation) -> Self {
        self.location = location;
        self
    }

    pub fn with_lifecycle(mut self, lifecycle: NotifyLifecycle) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    pub fn with_stack(mut self, stack: NotifyStack) -> Self {
        self.stack = stack;
        self
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Builder helper: make this a toast with default TTL.
    pub fn toast(self) -> Self {
        let ttl = match self.level {
            NotifyLevel::Success => Duration::from_secs(2),
            NotifyLevel::Info => Duration::from_secs(3),
            NotifyLevel::Warn => Duration::from_secs(5),
            NotifyLevel::Error => Duration::from_secs(8),
            NotifyLevel::Debug => Duration::from_secs(2),
        };
        self.with_location(NotifyLocation::Toast)
            .with_lifecycle(NotifyLifecycle::Ttl(ttl))
    }
}

// ── Sink trait ─────────────────────────────────────────────────────

pub trait NotifySink: Send + Sync {
    fn emit(&self, note: &Notification);
}

// ── Global notifier ────────────────────────────────────────────────

static GLOBAL_SINK: RwLock<Option<Arc<dyn NotifySink>>> = RwLock::new(None);

pub fn install(sink: Arc<dyn NotifySink>) {
    *GLOBAL_SINK.write().unwrap() = Some(sink);
}

pub fn global() -> Arc<dyn NotifySink> {
    GLOBAL_SINK
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| Arc::new(NoopSink))
}

/// RAII guard: installs a sink and restores the previous one on drop.
pub struct ScopedSink {
    prev: Option<Arc<dyn NotifySink>>,
}

impl ScopedSink {
    /// Install NoopSink for TUI mode — prevents stderr from leaking into raw terminal.
    pub fn tui() -> Self {
        let mut sink = GLOBAL_SINK.write().unwrap();
        let prev = sink.take();
        *sink = Some(Arc::new(NoopSink));
        Self { prev }
    }
}

impl Drop for ScopedSink {
    fn drop(&mut self) {
        let mut sink = GLOBAL_SINK.write().unwrap();
        *sink = self.prev.take();
    }
}

// ── Built-in sinks ─────────────────────────────────────────────────

pub struct NoopSink;
impl NotifySink for NoopSink {
    fn emit(&self, _note: &Notification) {}
}

/// Sink that prints to stdout/stderr based on level.
pub struct CliSink;
impl NotifySink for CliSink {
    fn emit(&self, note: &Notification) {
        let prefix = format!(
            "[atman] {}",
            if let Some(ref t) = note.target {
                format!("[{}] ", t)
            } else {
                String::new()
            }
        );
        let line = format!("{}{}", prefix, note.message);
        match note.level {
            NotifyLevel::Error | NotifyLevel::Warn => eprintln!("{line}"),
            NotifyLevel::Info | NotifyLevel::Success | NotifyLevel::Debug => println!("{line}"),
        }
    }
}

/// Fan-out to multiple sinks.
pub struct CompositeSink {
    sinks: Vec<Arc<dyn NotifySink>>,
}

impl CompositeSink {
    pub fn new(sinks: Vec<Arc<dyn NotifySink>>) -> Self {
        Self { sinks }
    }
}

impl NotifySink for CompositeSink {
    fn emit(&self, note: &Notification) {
        for s in &self.sinks {
            s.emit(note);
        }
    }
}

// ── Convenience constructors for NotifyStack ─────────────────────────

pub fn dedupe(key: impl Into<String>, window_ms: u64) -> NotifyStack {
    NotifyStack::Dedupe {
        key: key.into(),
        window: Duration::from_millis(window_ms),
    }
}

pub fn replace(key: impl Into<String>) -> NotifyStack {
    NotifyStack::Replace { key: key.into() }
}

pub fn merge_count(key: impl Into<String>, window_ms: u64) -> NotifyStack {
    NotifyStack::MergeCount {
        key: key.into(),
        window: Duration::from_millis(window_ms),
    }
}

pub fn coalesce(key: impl Into<String>) -> NotifyStack {
    NotifyStack::Coalesce { key: key.into() }
}

// ── notify! macro ──────────────────────────────────────────────────

/// Emit a notification through the global notifier.
///
/// # Syntax
///
/// ```ignore
/// notify!(level, "message {}", arg);
/// notify!(level, location = Toast, "message");
/// notify!(level, location = Status, stack = replace("key"), "message");
/// notify!(debug, target: "auto_snapshot", "snapshot {}", name);
/// ```
#[macro_export]
macro_rules! notify {
    (@level error) => { $crate::notify::NotifyLevel::Error };
    (@level warn) => { $crate::notify::NotifyLevel::Warn };
    (@level info) => { $crate::notify::NotifyLevel::Info };
    (@level success) => { $crate::notify::NotifyLevel::Success };
    (@level debug) => { $crate::notify::NotifyLevel::Debug };

    // Level only: notify!(level, "message", args...)
    ($level:ident, $msg:expr $(, $arg:expr)* $(,)?) => {{
        $crate::notify::global().emit(&$crate::notify::Notification::new(
            $crate::notify!(@level $level),
            format!($msg $(, $arg)*),
        ));
    }};

    // Level + location: notify!(level, location = Loc, "message", args...)
    ($level:ident, location = $loc:ident, $msg:expr $(, $arg:expr)* $(,)?) => {{
        $crate::notify::global().emit(
            &$crate::notify::Notification::new(
                $crate::notify!(@level $level),
                format!($msg $(, $arg)*),
            )
            .with_location($crate::notify::NotifyLocation::$loc),
        );
    }};

    // Level + location + stack: notify!(level, location = Loc, stack = fn("key"), "msg", args...)
    ($level:ident, location = $loc:ident, stack = $stack:ident ($($sk:expr),+), $msg:expr $(, $arg:expr)* $(,)?) => {{
        $crate::notify::global().emit(
            &$crate::notify::Notification::new(
                $crate::notify!(@level $level),
                format!($msg $(, $arg)*),
            )
            .with_location($crate::notify::NotifyLocation::$loc)
            .with_stack($crate::notify::$stack($($sk),+)),
        );
    }};

    // Level + location + lifecycle: notify!(level, location = Loc, lifecycle = Lc, "msg")
    ($level:ident, location = $loc:ident, lifecycle = $lc:ident, $msg:expr $(, $arg:expr)* $(,)?) => {{
        $crate::notify::global().emit(
            &$crate::notify::Notification::new(
                $crate::notify!(@level $level),
                format!($msg $(, $arg)*),
            )
            .with_location($crate::notify::NotifyLocation::$loc)
            .with_lifecycle($crate::notify::NotifyLifecycle::$lc),
        );
    }};

    // Debug + target: notify!(debug, target: "target_name", "msg", args...)
    ($level:ident, target: $target:expr, $msg:expr $(, $arg:expr)* $(,)?) => {{
        $crate::notify::global().emit(
            &$crate::notify::Notification::new(
                $crate::notify!(@level $level),
                format!($msg $(, $arg)*),
            )
            .with_target($target)
            .with_location($crate::notify::NotifyLocation::Log),
        );
    }};

    // Bare: notify!("message", args...)
    ($msg:expr $(, $arg:expr)* $(,)?) => {{
        $crate::notify::global().emit(&$crate::notify::Notification::new(
            $crate::notify::NotifyLevel::Info,
            format!($msg $(, $arg)*),
        ));
    }};
}

pub use notify;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_builder_defaults() {
        let n = Notification::new(NotifyLevel::Warn, "test message");
        assert_eq!(n.level, NotifyLevel::Warn);
        assert_eq!(n.location, NotifyLocation::Inline);
        assert!(matches!(n.lifecycle, NotifyLifecycle::Persistent));
        assert!(matches!(n.stack, NotifyStack::Append));
        assert_eq!(n.message, "test message");
    }

    #[test]
    fn toast_ttl_by_level() {
        let n = Notification::new(NotifyLevel::Success, "ok").toast();
        assert_eq!(n.location, NotifyLocation::Toast);
        assert_eq!(n.lifecycle, NotifyLifecycle::Ttl(Duration::from_secs(2)));

        let n = Notification::new(NotifyLevel::Error, "fail").toast();
        assert_eq!(n.lifecycle, NotifyLifecycle::Ttl(Duration::from_secs(8)));
    }

    #[test]
    fn stack_helpers() {
        assert_eq!(
            dedupe("my-key", 5000),
            NotifyStack::Dedupe {
                key: "my-key".into(),
                window: Duration::from_millis(5000),
            }
        );
        assert_eq!(replace("slot"), NotifyStack::Replace { key: "slot".into() });
        assert_eq!(
            merge_count("lag", 300),
            NotifyStack::MergeCount {
                key: "lag".into(),
                window: Duration::from_millis(300),
            }
        );
    }

    #[test]
    fn cli_sink_routes_by_level() {
        let n = Notification::new(NotifyLevel::Error, "boom");
        // CliSink sends Error to stderr — this test just ensures no panic.
        let sink = CliSink;
        sink.emit(&n);
    }

    #[test]
    fn composite_fanout() {
        let sink = CompositeSink::new(vec![
            Arc::new(NoopSink),
            Arc::new(CliSink),
        ]);
        let n = Notification::new(NotifyLevel::Info, "hello");
        sink.emit(&n); // no panic
    }

    #[test]
    fn level_as_str() {
        assert_eq!(NotifyLevel::Error.as_str(), "ERROR");
        assert_eq!(NotifyLevel::Success.as_str(), "SUCCESS");
        assert_eq!(NotifyLevel::Debug.as_str(), "DEBUG");
    }
}
