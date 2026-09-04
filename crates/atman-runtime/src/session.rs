#[cfg(test)]
use crate::context_state::checkpoint_epoch_digest;
use crate::context_state::{CompactionState, ContextState};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::event::{Event, EventSink, FlowRunId, TurnId};
#[cfg(test)]
use crate::event_log::reader::replay_context_snapshot_from;
use crate::event_log::replay::{SessionReplay, TranscriptReplayObserver};
use crate::event_writer::EventWriter;
use crate::injection::{Injection, InjectionId};
use crate::message::{Message, MessageRole};
use crate::projection::message_window::replay_transcript_from;
#[cfg(test)]
use crate::projection::message_window::{
    TranscriptEntry, replay_all_messages_with_seq, replay_messages_from,
};
use crate::stream::StreamFrame;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

fn is_auto_name_threshold(mut count: u64) -> bool {
    if count < 3 {
        return false;
    }
    while count % 3 == 0 {
        count /= 3;
    }
    count == 1
}

impl SessionId {
    pub fn now() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(s: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(s).map(Self)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

type WatchKeepalive = (
    watch::Receiver<ContextSnapshot>,
    watch::Receiver<Option<String>>,
    watch::Receiver<usize>,
    watch::Receiver<Vec<crate::memory::todo::Todo>>,
    watch::Receiver<Vec<crate::memory::plan::Plan>>,
);

#[derive(Debug)]
struct TurnState {
    flow_cancel: CancellationToken,
    streamed: bool,
}

pub struct WatchHub {
    pub stream_tx: broadcast::Sender<StreamFrame>,
    pub context: watch::Sender<ContextSnapshot>,
    pub goal: watch::Sender<Option<String>>,
    pub attach: watch::Sender<usize>,
    pub todos: watch::Sender<Vec<crate::memory::todo::Todo>>,
    pub plans: watch::Sender<Vec<crate::memory::plan::Plan>>,
    _keepalive: WatchKeepalive,
}

pub struct InteractionServices {
    pub approval: std::sync::Arc<ApprovalRegistry>,
    pub compact_reviews: std::sync::Arc<CompactReviewRegistry>,
    pub forms: std::sync::Arc<FormRegistry>,
}

impl InteractionServices {
    fn new(sink: &EventSink) -> Self {
        Self {
            approval: std::sync::Arc::new(ApprovalRegistry::new()),
            compact_reviews: std::sync::Arc::new(CompactReviewRegistry::new_with_event_sink(
                sink.clone(),
            )),
            forms: std::sync::Arc::new(FormRegistry::new_with_event_sink(sink.clone())),
        }
    }
}

pub struct Session {
    id: SessionId,
    dir: PathBuf,
    writer: std::sync::Mutex<Option<EventWriter>>,
    sink: EventSink,
    context: std::sync::Arc<ContextState>,
    turns: Mutex<HashMap<TurnId, TurnState>>,
    pub watch: WatchHub,
    pub watch_hub: std::sync::Arc<crate::watch::WatchHub>,
    pub flow_registry: std::sync::Arc<crate::tools::agent_ctrl::FlowRegistry>,
    /// Broker for the structured permission pipeline. Bound to `flow_registry`
    /// so identity authentication and terminal cleanup observe the same runs.
    pub permission_broker: std::sync::Arc<crate::permission::PermissionBroker>,
    trust: watch::Sender<crate::trust::TrustConfig>,
    trust_update_lock: std::sync::Mutex<()>,
    /// Handle of the current root FlowRun; set per turn.
    current_root: std::sync::Mutex<Option<String>>,
    successful_flow_count: std::sync::atomic::AtomicU64,
    pub interactions: InteractionServices,
    injection_queue: std::sync::Arc<crate::injection::InjectionQueue>,
    last_image_user_msg: Mutex<Option<LastImageUserMsg>>,
    pending_images: Mutex<Vec<crate::message::ImageSource>>,
    read_files: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    output_store: std::sync::Arc<crate::tools::tool_output::OutputStore>,
    tool_output_budget: Mutex<crate::tools::tool_output::ToolOutputBudget>,
    fs_access_mode: Mutex<Option<crate::fs_access::FsAccessMode>>,
    project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PendingCompactReview {
    pub review_id: String,
    pub summary: String,
    pub slice_preview: String,
    pub slice_count: usize,
    pub range_start: usize,
    pub range_end: usize,
    pub tokens_before: u64,
    pub emitted_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum CompactReviewDecision {
    AcceptAsIs,
    AcceptEdited { summary: String },
    Reject,
}

pub struct CompactReviewRegistry {
    entry: std::sync::Mutex<Option<CompactReviewEntry>>,
    watch_tx: watch::Sender<Option<PendingCompactReview>>,
    event_sink: Option<EventSink>,
}

struct CompactReviewEntry {
    pending: PendingCompactReview,
    responder: tokio::sync::oneshot::Sender<CompactReviewDecision>,
}

pub struct CompactReviewResolutionCommit {
    pub event: Option<crate::event::EventEnvelope>,
}

impl Default for CompactReviewRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CompactReviewRegistry {
    pub fn new() -> Self {
        let (watch_tx, _) = watch::channel(None);
        Self {
            entry: std::sync::Mutex::new(None),
            watch_tx,
            event_sink: None,
        }
    }

    fn new_with_event_sink(event_sink: EventSink) -> Self {
        let mut registry = Self::new();
        registry.event_sink = Some(event_sink);
        registry
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<PendingCompactReview>> {
        self.watch_tx.subscribe()
    }

    pub fn list_pending(&self) -> Option<PendingCompactReview> {
        self.entry
            .lock()
            .unwrap()
            .as_ref()
            .map(|e| e.pending.clone())
    }

    pub fn subscriber_count(&self) -> usize {
        self.watch_tx.receiver_count()
    }

    pub fn request(
        &self,
        pending: PendingCompactReview,
    ) -> tokio::sync::oneshot::Receiver<CompactReviewDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.watch_tx.receiver_count() == 0 {
            let _ = tx.send(CompactReviewDecision::AcceptAsIs);
            return rx;
        }
        {
            let mut slot = self.entry.lock().unwrap();
            if let Some(prev) = slot.take() {
                if let Some(sink) = &self.event_sink {
                    sink.emit(crate::event::Event::CompactReviewResolved {
                        review_id: prev.pending.review_id.clone(),
                        decision: CompactReviewDecision::Reject,
                        abandoned: true,
                    });
                }
                let _ = prev.responder.send(CompactReviewDecision::Reject);
            }
            *slot = Some(CompactReviewEntry {
                pending: pending.clone(),
                responder: tx,
            });
        }
        if let Some(sink) = &self.event_sink {
            sink.emit(crate::event::Event::CompactReviewRequested {
                review: pending.clone(),
            });
        }
        let _ = self.watch_tx.send(Some(pending));
        rx
    }

    pub fn decide(&self, review_id: &str, decision: CompactReviewDecision) -> bool {
        self.decide_with_commit(review_id, decision).is_some()
    }

    pub fn decide_with_commit(
        &self,
        review_id: &str,
        decision: CompactReviewDecision,
    ) -> Option<CompactReviewResolutionCommit> {
        let entry = {
            let mut slot = self.entry.lock().unwrap();
            match slot.as_ref() {
                Some(e) if e.pending.review_id == review_id => slot.take(),
                _ => None,
            }
        };
        match entry {
            Some(e) => {
                let event = self.event_sink.as_ref().map(|sink| {
                    sink.emit_returning_envelope(crate::event::Event::CompactReviewResolved {
                        review_id: review_id.to_owned(),
                        decision: decision.clone(),
                        abandoned: false,
                    })
                });
                let _ = e.responder.send(decision);
                let _ = self.watch_tx.send(None);
                Some(CompactReviewResolutionCommit { event })
            }
            None => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub tool_use_id: String,
    pub tool_name: String,
    pub args_preview: String,
    pub preview: Option<String>,
    pub level: crate::tool::ApprovalLevel,
    pub run_id: FlowRunId,
    pub emitted_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    Approve,
    Deny { reason: String },
}

pub struct FormRegistry {
    entries: std::sync::Mutex<Vec<FormEntry>>,
    watch_tx: watch::Sender<Vec<crate::form::PendingForm>>,
    event_sink: Option<EventSink>,
}

struct FormEntry {
    pending: crate::form::PendingForm,
    responder: tokio::sync::oneshot::Sender<crate::form::FormSubmission>,
}

pub struct FormResolutionCommit {
    pub event: Option<crate::event::EventEnvelope>,
}

impl Default for FormRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FormRegistry {
    pub fn new() -> Self {
        let (watch_tx, _) = watch::channel(Vec::new());
        Self {
            entries: std::sync::Mutex::new(Vec::new()),
            watch_tx,
            event_sink: None,
        }
    }

    fn new_with_event_sink(event_sink: EventSink) -> Self {
        let mut registry = Self::new();
        registry.event_sink = Some(event_sink);
        registry
    }

    pub fn subscribe(&self) -> watch::Receiver<Vec<crate::form::PendingForm>> {
        self.watch_tx.subscribe()
    }

    pub fn list_pending(&self) -> Vec<crate::form::PendingForm> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.pending.clone())
            .collect()
    }

    pub fn subscriber_count(&self) -> usize {
        self.watch_tx.receiver_count()
    }

    // No TUI attached → auto-cancel so flows don't hang forever. Otherwise
    // enqueue and hand a receiver back to the caller.
    pub fn request(
        &self,
        pending: crate::form::PendingForm,
    ) -> tokio::sync::oneshot::Receiver<crate::form::FormSubmission> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.watch_tx.receiver_count() == 0 {
            let _ = tx.send(crate::form::FormSubmission::Rejected);
            return rx;
        }
        {
            let mut entries = self.entries.lock().unwrap();
            entries.push(FormEntry {
                pending: pending.clone(),
                responder: tx,
            });
        }
        if let Some(sink) = &self.event_sink {
            sink.emit(crate::event::Event::FormRequested {
                form: pending.clone(),
            });
        }
        self.broadcast_snapshot();
        rx
    }

    pub fn submit(&self, form_id: &str, submission: crate::form::FormSubmission) -> bool {
        matches!(self.submit_with_commit(form_id, submission), Ok(Some(_)))
    }

    pub fn submit_with_commit(
        &self,
        form_id: &str,
        submission: crate::form::FormSubmission,
    ) -> Result<Option<FormResolutionCommit>, String> {
        self.resolve(form_id, submission, false)
    }

    fn resolve(
        &self,
        form_id: &str,
        submission: crate::form::FormSubmission,
        abandoned: bool,
    ) -> Result<Option<FormResolutionCommit>, String> {
        let entry = {
            let mut entries = self.entries.lock().unwrap();
            let pos = entries.iter().position(|e| e.pending.form_id == form_id);
            match pos {
                Some(pos) => {
                    entries[pos].pending.form.validate_submission(&submission)?;
                    Some(entries.remove(pos))
                }
                None => None,
            }
        };
        match entry {
            Some(e) => {
                let event = self.event_sink.as_ref().map(|sink| {
                    sink.emit_returning_envelope(crate::event::Event::FormResolved {
                        form_id: form_id.to_owned(),
                        run_id: e.pending.run_id.clone(),
                        submission: submission.clone(),
                        abandoned,
                    })
                });
                let _ = e.responder.send(submission);
                self.broadcast_snapshot();
                Ok(Some(FormResolutionCommit { event }))
            }
            None => Ok(None),
        }
    }

    pub fn cancel(&self, form_id: &str) -> bool {
        self.submit(form_id, crate::form::FormSubmission::Rejected)
    }

    pub fn cancel_all(&self) {
        let drained: Vec<FormEntry> = {
            let mut entries = self.entries.lock().unwrap();
            std::mem::take(&mut *entries)
        };
        for entry in drained {
            if let Some(sink) = &self.event_sink {
                sink.emit(crate::event::Event::FormResolved {
                    form_id: entry.pending.form_id,
                    run_id: entry.pending.run_id,
                    submission: crate::form::FormSubmission::Rejected,
                    abandoned: true,
                });
            }
            let _ = entry.responder.send(crate::form::FormSubmission::Rejected);
        }
        self.broadcast_snapshot();
    }

    pub fn promote(&self, form_id: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(pos) = entries.iter().position(|e| e.pending.form_id == form_id) {
            if pos == 0 {
                return;
            }
            let entry = entries.remove(pos);
            entries.insert(0, entry);
        }
        drop(entries);
        self.broadcast_snapshot();
    }

    fn broadcast_snapshot(&self) {
        let snap = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.pending.clone())
            .collect();
        let _ = self.watch_tx.send(snap);
    }
}

pub struct ApprovalRegistry {
    entries: std::sync::Mutex<Vec<ApprovalEntry>>,
    watch_tx: watch::Sender<Vec<PendingApproval>>,
    next_entry_id: std::sync::atomic::AtomicU64,
}

struct ApprovalEntry {
    entry_id: u64,
    pending: PendingApproval,
    responder: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

/// Identifies one queued approval entry. Providers can reuse a `tool_use_id`
/// across concurrent runs, so cleanup must target the exact entry it created
/// rather than the first entry that happens to share the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalTicket(u64);

impl Default for ApprovalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        let (watch_tx, _) = watch::channel(Vec::new());
        Self {
            entries: std::sync::Mutex::new(Vec::new()),
            watch_tx,
            next_entry_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Vec<PendingApproval>> {
        self.watch_tx.subscribe()
    }

    pub fn has_subscribers(&self) -> bool {
        self.watch_tx.receiver_count() > 0
    }

    pub fn list_pending(&self) -> Vec<PendingApproval> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.pending.clone())
            .collect()
    }

    pub fn request(
        &self,
        pending: PendingApproval,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision> {
        self.request_tracked(pending).1
    }

    /// Same as [`Self::request`], but also returns a ticket that identifies this
    /// exact queue entry for later [`Self::cancel`].
    pub fn request_tracked(
        &self,
        pending: PendingApproval,
    ) -> (
        Option<ApprovalTicket>,
        tokio::sync::oneshot::Receiver<ApprovalDecision>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let entry_id = self
            .next_entry_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut entries = self.entries.lock().unwrap();
            entries.push(ApprovalEntry {
                entry_id,
                pending,
                responder: tx,
            });
        }
        self.broadcast_snapshot();
        (Some(ApprovalTicket(entry_id)), rx)
    }

    /// Removes one queued entry and denies it, so a request already settled
    /// elsewhere (broker cancellation, flow terminal) cannot linger in the UI.
    pub fn cancel(&self, ticket: ApprovalTicket, reason: impl Into<String>) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let Some(pos) = entries.iter().position(|e| e.entry_id == ticket.0) else {
            return false;
        };
        let entry = entries.remove(pos);
        let _ = entry.responder.send(ApprovalDecision::Deny {
            reason: reason.into(),
        });
        drop(entries);
        self.broadcast_snapshot();
        true
    }

    pub fn decide(&self, tool_use_id: &str, decision: ApprovalDecision) -> bool {
        let mut entries = self.entries.lock().unwrap();
        if let Some(pos) = entries
            .iter()
            .position(|e| e.pending.tool_use_id == tool_use_id)
        {
            let entry = entries.remove(pos);
            let _ = entry.responder.send(decision);
            drop(entries);
            self.broadcast_snapshot();
            true
        } else {
            false
        }
    }

    pub fn decide_all(&self, decision: ApprovalDecision) -> usize {
        let mut entries = self.entries.lock().unwrap();
        let count = entries.len();
        for entry in entries.drain(..) {
            let _ = entry.responder.send(decision.clone());
        }
        drop(entries);
        self.broadcast_snapshot();
        count
    }

    fn broadcast_snapshot(&self) {
        let snapshot = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.pending.clone())
            .collect();
        let _ = self.watch_tx.send(snapshot);
    }
}
type ImagePart = (usize, String);

#[derive(Debug, Clone)]
struct LastImageUserMsg {
    message_seq: u64,
    message_turn_id: crate::event::TurnId,
    images: Vec<ImagePart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactReviewMode {
    Always,
    #[default]
    ManualOnly,
    Never,
}

impl CompactReviewMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "always" => Some(Self::Always),
            "manual-only" | "manual_only" => Some(Self::ManualOnly),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    pub fn should_review(self, forced: bool) -> bool {
        match self {
            Self::Always => true,
            Self::ManualOnly => forced,
            Self::Never => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactResult {
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub compacted_start: usize,
    pub compacted_end: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextUsageBucket {
    pub provider: String,
    pub model: String,
    pub call_purpose: crate::context_plan::ContextCallPurpose,
    pub call_scope: crate::context_plan::ContextCallScope,
    pub calls: u64,
    /// Total prompt input, including cache hits.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl ContextUsageBucket {
    pub fn is_primary(&self) -> bool {
        self.call_purpose == crate::context_plan::ContextCallPurpose::General
            && self.call_scope == crate::context_plan::ContextCallScope::Root
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextSnapshot {
    pub model: String,
    pub provider: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub mcp_servers: Vec<crate::mcp::McpServerStatus>,
    pub memory_recent_count: u16,
    pub window_tokens: u64,
    pub window_budget: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub last_ttft_ms: u64,
    pub last_tokens_per_sec: f64,
    pub usage_buckets: Vec<ContextUsageBucket>,
}

impl ContextSnapshot {
    pub fn primary_usage(&self) -> Option<&ContextUsageBucket> {
        self.usage_buckets.iter().find(|bucket| {
            bucket.is_primary()
                && bucket.model == self.model
                && (self.provider.is_empty() || bucket.provider == self.provider)
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionOpenError {
    #[error("invalid session id `{sid}` (want a UUID)")]
    InvalidId { sid: String },
    #[error("session `{sid}` not found at {}", dir.display())]
    NotFound { sid: String, dir: PathBuf },
    #[error("session writer init: {0}")]
    WriterInit(#[source] std::io::Error),
    #[error("replay {}: {source}", path.display())]
    Replay {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("load session trust {}: {source}", path.display())]
    Trust {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct RestoredSession {
    pub session: Session,
    pub events: Vec<crate::event::EventEnvelope>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustUpdateError {
    #[error("persist session trust: {0}")]
    Session(#[source] std::io::Error),
    #[error("persist global trust: {0}")]
    Global(#[source] std::io::Error),
    #[error(
        "persist global trust failed ({global}); rollback session trust also failed ({rollback})"
    )]
    RollbackFailed {
        global: std::io::Error,
        rollback: std::io::Error,
    },
}

fn trust_path(dir: &Path) -> PathBuf {
    dir.join("trust.json")
}

fn read_trust(dir: &Path) -> std::io::Result<crate::trust::TrustConfig> {
    let path = trust_path(dir);
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub fn load_session_trust(
    dir: impl AsRef<Path>,
) -> std::io::Result<Option<crate::trust::TrustConfig>> {
    match read_trust(dir.as_ref()) {
        Ok(trust) => Ok(Some(trust)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_trust(dir: &Path, trust: &crate::trust::TrustConfig) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(trust)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temp = dir.join(format!(".trust.{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes)?;
    if let Err(error) = std::fs::rename(&temp, trust_path(dir)) {
        let _ = std::fs::remove_file(temp);
        return Err(error);
    }
    Ok(())
}

fn load_goal(dir: &Path) -> Option<String> {
    if dir.as_os_str().is_empty() {
        return None;
    }
    let store = crate::memory::goal::GoalStore::at(dir);
    match store.get() {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PersistedContextState {
    #[serde(default)]
    model: String,
    #[serde(default)]
    window_tokens: u64,
    #[serde(default)]
    window_budget: u64,
}

impl PersistedContextState {
    fn path(dir: &Path) -> PathBuf {
        dir.join("context_state.json")
    }

    fn load(dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(dir)) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    fn save(&self, dir: &Path) {
        if dir.as_os_str().is_empty() {
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(dir), &json);
        }
    }
}

/// Builds the flow registry and its permission broker as one unit. The broker
/// authenticates requesters against this exact registry, so both must be the
/// same Arc for every session constructor.
fn new_permission_pipeline(
    sink: &EventSink,
    stream: &broadcast::Sender<StreamFrame>,
) -> (
    std::sync::Arc<crate::tools::agent_ctrl::FlowRegistry>,
    std::sync::Arc<crate::permission::PermissionBroker>,
) {
    let flow_registry = std::sync::Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
    let broker = crate::permission::PermissionBroker::shared(std::sync::Arc::clone(&flow_registry));
    broker.set_audit_projector(crate::permission_audit::PermissionAuditProjector::new(
        sink.clone(),
        stream.clone(),
    ));
    (flow_registry, broker)
}

fn default_project_index(root: &Path) -> Option<std::sync::Arc<crate::index::AnchorIndex>> {
    match crate::index::AnchorIndex::open_project(root) {
        Ok(idx) => Some(std::sync::Arc::new(idx)),
        Err(e) => {
            crate::notify!(
                warn,
                "project index unavailable at {} — history search disabled: {e}",
                root.display()
            );
            None
        }
    }
}

impl Session {
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::open_with_redactor(root, None)
    }

    pub fn open_with_trust(
        root: impl AsRef<Path>,
        trust: crate::trust::TrustConfig,
    ) -> std::io::Result<Self> {
        let root_ref = root.as_ref();
        let project_index = default_project_index(root_ref);
        Self::open_with_context_and_trust(root_ref, None, project_index, trust)
    }

    pub fn open_with_redactor(
        root: impl AsRef<Path>,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
    ) -> std::io::Result<Self> {
        let root_ref = root.as_ref();
        let project_index = default_project_index(root_ref);
        Self::open_with_context_and_trust(
            root_ref,
            redactor,
            project_index,
            crate::trust::TrustConfig::default(),
        )
    }

    pub fn open_with_context_and_trust(
        root: impl AsRef<Path>,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
        trust: crate::trust::TrustConfig,
    ) -> std::io::Result<Self> {
        let session = Self::open_with_context_inner(root, redactor, project_index)?;
        write_trust(&session.dir, &trust)?;
        session.trust.send_replace(trust);
        Ok(session)
    }

    pub fn open_with_context(
        root: impl AsRef<Path>,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
    ) -> std::io::Result<Self> {
        Self::open_with_context_and_trust(
            root,
            redactor,
            project_index,
            crate::trust::TrustConfig::default(),
        )
    }

    fn open_with_context_inner(
        root: impl AsRef<Path>,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
    ) -> std::io::Result<Self> {
        let id = SessionId::now();
        let dir = root.as_ref().join("sessions").join(id.to_string());
        if let Some(ls) = crate::notify::log_sink() {
            ls.set_session_id(Some(id.to_string()));
        }
        let writer = EventWriter::spawn_full(
            &dir,
            redactor.clone(),
            project_index.clone(),
            Some(id.to_string()),
        )?;
        if let Err(e) = crate::session_meta::SessionMeta::from_cwd().save(&dir) {
            crate::notify!(error, "session meta write failed: {e}");
        }
        let mut sink = EventSink::new().with_forwarder(writer.sender());
        if let Some(r) = redactor {
            sink = sink.with_redactor(r);
        }
        let injection_queue = crate::injection::InjectionQueue::new(Some(sink.clone()));
        let (stream_tx, _) = broadcast::channel(2048);
        let (context_watch, context_rx) = watch::channel(ContextSnapshot::default());
        let (goal_watch, goal_rx) = watch::channel(None);
        let (attach_watch, attach_rx) = watch::channel(0);
        let (todos_watch, todos_rx) = watch::channel(Vec::new());
        let (plans_watch, plans_rx) = watch::channel(Vec::new());
        let events_handle = sink.events_handle();
        let output_store = std::sync::Arc::new(crate::tools::tool_output::OutputStore::at(&dir));
        let (flow_registry, permission_broker) = new_permission_pipeline(&sink, &stream_tx);
        let interactions = InteractionServices::new(&sink);
        Ok(Self {
            id,
            dir,
            writer: std::sync::Mutex::new(Some(writer)),
            sink,
            context: std::sync::Arc::new(ContextState::from_stream(
                crate::message_stream::MessageStream::new(events_handle),
                Vec::new(),
                CompactionState::new(),
            )),
            output_store: output_store.clone(),
            tool_output_budget: Mutex::new(Default::default()),
            turns: Mutex::new(HashMap::new()),
            watch: WatchHub {
                stream_tx,
                context: context_watch,
                goal: goal_watch,
                attach: attach_watch,
                todos: todos_watch,
                plans: plans_watch,
                _keepalive: (context_rx, goal_rx, attach_rx, todos_rx, plans_rx),
            },
            watch_hub: std::sync::Arc::new(crate::watch::WatchHub::new()),
            flow_registry,
            permission_broker,
            trust: watch::channel(crate::trust::TrustConfig::default()).0,
            trust_update_lock: std::sync::Mutex::new(()),
            current_root: std::sync::Mutex::new(None),
            successful_flow_count: std::sync::atomic::AtomicU64::new(0),
            interactions,
            injection_queue,
            last_image_user_msg: Mutex::new(None),
            pending_images: Mutex::new(Vec::new()),
            read_files: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashSet::new()),
            ),
            fs_access_mode: Mutex::new(None),
            project_index,
        })
    }

    pub fn open_existing(root: impl AsRef<Path>, sid: &str) -> Result<Self, SessionOpenError> {
        Self::open_existing_with_redactor(root, sid, None)
    }

    pub fn open_existing_with_trust(
        root: impl AsRef<Path>,
        sid: &str,
        trust: crate::trust::TrustConfig,
    ) -> Result<Self, SessionOpenError> {
        let root_ref = root.as_ref();
        let project_index = default_project_index(root_ref);
        Self::open_existing_with_context_and_trust(root_ref, sid, None, project_index, trust)
    }

    pub fn open_existing_with_redactor(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
    ) -> Result<Self, SessionOpenError> {
        let project_index = default_project_index(root.as_ref());
        Self::open_existing_with_context_and_trust(
            root,
            sid,
            redactor,
            project_index,
            crate::trust::TrustConfig::default(),
        )
    }

    pub fn open_existing_with_context_and_trust(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
        global_trust: crate::trust::TrustConfig,
    ) -> Result<Self, SessionOpenError> {
        Ok(Self::restore_existing_with_context_trust_and_observer(
            root,
            sid,
            redactor,
            project_index,
            global_trust,
            None,
        )?
        .session)
    }

    pub fn restore_existing_with_context_and_trust(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
        global_trust: crate::trust::TrustConfig,
    ) -> Result<RestoredSession, SessionOpenError> {
        Self::restore_existing_with_context_trust_and_observer(
            root,
            sid,
            redactor,
            project_index,
            global_trust,
            None,
        )
    }

    pub fn open_existing_with_replay_observer(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
        global_trust: crate::trust::TrustConfig,
        observer: &mut dyn TranscriptReplayObserver,
    ) -> Result<Self, SessionOpenError> {
        Ok(Self::restore_existing_with_context_trust_and_observer(
            root,
            sid,
            redactor,
            project_index,
            global_trust,
            Some(observer),
        )?
        .session)
    }

    fn restore_existing_with_context_trust_and_observer(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
        global_trust: crate::trust::TrustConfig,
        observer: Option<&mut dyn TranscriptReplayObserver>,
    ) -> Result<RestoredSession, SessionOpenError> {
        let restored = Self::restore_existing_with_context_inner(
            root,
            sid,
            redactor,
            project_index,
            observer,
        )?;
        let path = trust_path(&restored.session.dir);
        let trust = if let Some(trust) =
            load_session_trust(&restored.session.dir).map_err(|source| SessionOpenError::Trust {
                path: path.clone(),
                source,
            })? {
            trust
        } else {
            write_trust(&restored.session.dir, &global_trust).map_err(|source| {
                SessionOpenError::Trust {
                    path: path.clone(),
                    source,
                }
            })?;
            global_trust
        };
        restored.session.trust.send_replace(trust);
        Ok(restored)
    }

    pub fn open_existing_with_context(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
    ) -> Result<Self, SessionOpenError> {
        Self::open_existing_with_context_and_trust(
            root,
            sid,
            redactor,
            project_index,
            crate::trust::TrustConfig::default(),
        )
    }

    fn restore_existing_with_context_inner(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
        project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
        observer: Option<&mut dyn TranscriptReplayObserver>,
    ) -> Result<RestoredSession, SessionOpenError> {
        let id = SessionId::parse(sid).map_err(|_| SessionOpenError::InvalidId {
            sid: sid.to_string(),
        })?;
        let dir = root.as_ref().join("sessions").join(id.to_string());
        if let Some(ls) = crate::notify::log_sink() {
            ls.set_session_id(Some(id.to_string()));
        }
        if !dir.exists() {
            return Err(SessionOpenError::NotFound {
                sid: sid.to_string(),
                dir: dir.clone(),
            });
        }
        let writer = EventWriter::spawn_full(
            &dir,
            redactor.clone(),
            project_index.clone(),
            Some(id.to_string()),
        )
        .map_err(SessionOpenError::WriterInit)?;
        let mut sink = EventSink::new().with_forwarder(writer.sender());
        if let Some(r) = redactor {
            sink = sink.with_redactor(r);
        }
        let events_path = dir.join("events.jsonl");
        let replay = SessionReplay::from_path(&events_path, observer)?;
        let initial_msgs = replay.compacted_messages;
        let messages = initial_msgs
            .iter()
            .map(|(_, message)| message.clone())
            .collect();
        let checkpoint_epoch = replay.checkpoint_epoch;
        let all_msgs = replay.all_messages;
        let events = replay.events;
        if let Some(last_seq) = replay.last_seq {
            sink.restore_seq(last_seq);
            writer.restore_durable_seq(last_seq);
        }
        let mut initial_context = replay.context;
        let persisted = PersistedContextState::load(&dir);
        if !persisted.model.is_empty() {
            initial_context.model = persisted.model;
        }
        initial_context.window_tokens = persisted.window_tokens;
        initial_context.window_budget = persisted.window_budget;
        let initial_goal = load_goal(&dir);
        let injection_queue = crate::injection::InjectionQueue::new(Some(sink.clone()));
        let (stream_tx, _) = broadcast::channel(2048);
        let (context_watch, context_rx) = watch::channel(initial_context);
        let (goal_watch, goal_rx) = watch::channel(initial_goal);
        let (attach_watch, attach_rx) = watch::channel(0);
        let (todos_watch, todos_rx) = watch::channel(Vec::new());
        let (plans_watch, plans_rx) = watch::channel(Vec::new());
        let events_handle = sink.events_handle();
        let output_store = std::sync::Arc::new(crate::tools::tool_output::OutputStore::at(&dir));
        let (flow_registry, permission_broker) = new_permission_pipeline(&sink, &stream_tx);
        let interactions = InteractionServices::new(&sink);
        let session = Self {
            id,
            dir,
            writer: std::sync::Mutex::new(Some(writer)),
            sink,
            context: std::sync::Arc::new(ContextState::from_stream(
                crate::message_stream::MessageStream::with_initial(
                    events_handle,
                    initial_msgs,
                    all_msgs,
                ),
                messages,
                {
                    let c = CompactionState::new();
                    c.restore_context_epoch(checkpoint_epoch);
                    if persisted.window_tokens > 0 {
                        c.model_window_tokens.store(
                            persisted.window_tokens,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    c
                },
            )),
            output_store: output_store.clone(),
            tool_output_budget: Mutex::new(Default::default()),
            turns: Mutex::new(HashMap::new()),
            watch: WatchHub {
                stream_tx,
                context: context_watch,
                goal: goal_watch,
                attach: attach_watch,
                todos: todos_watch,
                plans: plans_watch,
                _keepalive: (context_rx, goal_rx, attach_rx, todos_rx, plans_rx),
            },
            watch_hub: std::sync::Arc::new(crate::watch::WatchHub::new()),
            flow_registry,
            permission_broker,
            trust: watch::channel(crate::trust::TrustConfig::default()).0,
            trust_update_lock: std::sync::Mutex::new(()),
            current_root: std::sync::Mutex::new(None),
            successful_flow_count: std::sync::atomic::AtomicU64::new(0),
            interactions,
            injection_queue,
            last_image_user_msg: Mutex::new(None),
            pending_images: Mutex::new(Vec::new()),
            read_files: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashSet::new()),
            ),
            fs_access_mode: Mutex::new(None),
            project_index,
        };
        Ok(RestoredSession { session, events })
    }

    pub fn open_ephemeral() -> Self {
        let sink = EventSink::new();
        let injection_queue = crate::injection::InjectionQueue::new(Some(sink.clone()));
        let (stream_tx, _) = broadcast::channel(2048);
        let (context_watch, context_rx) = watch::channel(ContextSnapshot::default());
        let (goal_watch, goal_rx) = watch::channel(None);
        let (attach_watch, attach_rx) = watch::channel(0);
        let (todos_watch, todos_rx) = watch::channel(Vec::new());
        let (plans_watch, plans_rx) = watch::channel(Vec::new());
        let events_handle = sink.events_handle();
        let output_store = std::sync::Arc::new(crate::tools::tool_output::OutputStore::default());
        let (flow_registry, permission_broker) = new_permission_pipeline(&sink, &stream_tx);
        let interactions = InteractionServices::new(&sink);
        Self {
            id: SessionId::now(),
            dir: PathBuf::new(),
            writer: std::sync::Mutex::new(None),
            sink,
            context: std::sync::Arc::new(ContextState::from_stream(
                crate::message_stream::MessageStream::new(events_handle),
                Vec::new(),
                CompactionState::new(),
            )),
            output_store: output_store.clone(),
            tool_output_budget: Mutex::new(Default::default()),
            turns: Mutex::new(HashMap::new()),
            watch: WatchHub {
                stream_tx,
                context: context_watch,
                goal: goal_watch,
                attach: attach_watch,
                todos: todos_watch,
                plans: plans_watch,
                _keepalive: (context_rx, goal_rx, attach_rx, todos_rx, plans_rx),
            },
            watch_hub: std::sync::Arc::new(crate::watch::WatchHub::new()),
            flow_registry,
            permission_broker,
            trust: watch::channel(crate::trust::TrustConfig::default()).0,
            trust_update_lock: std::sync::Mutex::new(()),
            current_root: std::sync::Mutex::new(None),
            successful_flow_count: std::sync::atomic::AtomicU64::new(0),
            interactions,
            injection_queue,
            last_image_user_msg: Mutex::new(None),
            pending_images: Mutex::new(Vec::new()),
            read_files: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashSet::new()),
            ),
            fs_access_mode: Mutex::new(None),
            project_index: None,
        }
    }

    pub fn project_index(&self) -> Option<std::sync::Arc<crate::index::AnchorIndex>> {
        self.project_index.clone()
    }

    pub fn approval(&self) -> std::sync::Arc<ApprovalRegistry> {
        self.interactions.approval.clone()
    }

    pub fn permission_broker(&self) -> std::sync::Arc<crate::permission::PermissionBroker> {
        std::sync::Arc::clone(&self.permission_broker)
    }

    pub fn trust_config(&self) -> crate::trust::TrustConfig {
        self.trust.borrow().clone()
    }

    pub fn subscribe_trust(&self) -> watch::Receiver<crate::trust::TrustConfig> {
        self.trust.subscribe()
    }

    pub fn update_trust(
        &self,
        trust: crate::trust::TrustConfig,
        persist_global: impl FnOnce(&crate::trust::TrustConfig) -> std::io::Result<()>,
    ) -> Result<(), TrustUpdateError> {
        let _guard = self.trust_update_lock.lock().unwrap();
        let previous = self.trust_config();
        write_trust(&self.dir, &trust).map_err(TrustUpdateError::Session)?;
        if let Err(global) = persist_global(&trust) {
            return match write_trust(&self.dir, &previous) {
                Ok(()) => Err(TrustUpdateError::Global(global)),
                Err(rollback) => Err(TrustUpdateError::RollbackFailed { global, rollback }),
            };
        }
        self.trust.send_replace(trust);
        Ok(())
    }

    pub fn compact_reviews(&self) -> std::sync::Arc<CompactReviewRegistry> {
        self.interactions.compact_reviews.clone()
    }

    pub fn forms(&self) -> std::sync::Arc<FormRegistry> {
        self.interactions.forms.clone()
    }

    pub fn fs_access_mode(&self) -> Option<crate::fs_access::FsAccessMode> {
        *self.fs_access_mode.lock().unwrap()
    }

    pub fn set_fs_access_mode(&self, mode: crate::fs_access::FsAccessMode) {
        *self.fs_access_mode.lock().unwrap() = Some(mode);
    }

    pub fn compact_review_mode(&self) -> CompactReviewMode {
        *self.context.compaction.review_mode.lock().unwrap()
    }

    pub fn set_compact_review_mode(&self, mode: CompactReviewMode) {
        *self.context.compaction.review_mode.lock().unwrap() = mode;
    }

    pub fn read_files(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>> {
        self.read_files.clone()
    }

    pub fn output_store(&self) -> std::sync::Arc<crate::tools::tool_output::OutputStore> {
        self.output_store.clone()
    }

    pub fn set_tool_output_budget(&self, budget: crate::tools::tool_output::ToolOutputBudget) {
        *self.tool_output_budget.lock().unwrap() = budget;
    }

    pub fn tool_output_budget(&self) -> crate::tools::tool_output::ToolOutputBudget {
        *self.tool_output_budget.lock().unwrap()
    }

    pub fn mark_file_read(&self, path: &std::path::Path) {
        if let Ok(mut set) = self.read_files.lock() {
            set.insert(path.to_path_buf());
            if let Ok(canonical) = std::fs::canonicalize(path) {
                set.insert(canonical);
            }
        }
    }

    pub fn stream_tx(&self) -> broadcast::Sender<StreamFrame> {
        self.watch.stream_tx.clone()
    }

    pub fn set_current_root(&self, handle: String) {
        *self.current_root.lock().unwrap() = Some(handle);
    }

    pub fn current_root(&self) -> Option<String> {
        self.current_root.lock().unwrap().clone()
    }

    pub fn clear_current_root(&self) {
        *self.current_root.lock().unwrap() = None;
    }

    pub fn record_successful_flow(&self) -> Option<u64> {
        let count = self
            .successful_flow_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        is_auto_name_threshold(count).then_some(count)
    }

    pub fn successful_flow_count(&self) -> u64 {
        self.successful_flow_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn stream_subscribe(&self) -> broadcast::Receiver<StreamFrame> {
        self.watch.stream_tx.subscribe()
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn transcript_since_open(&self) -> Vec<crate::projection::message_window::TranscriptEntry> {
        crate::event_log::replay::transcript_from_envelopes(&self.sink.snapshot_envelopes())
    }

    pub fn transcript_replay(&self) -> Vec<crate::projection::message_window::TranscriptEntry> {
        let Some(path) = self.events_path() else {
            return Vec::new();
        };
        replay_transcript_from(&path).unwrap_or_default()
    }

    pub fn events_path(&self) -> Option<std::path::PathBuf> {
        self.writer
            .lock()
            .unwrap()
            .as_ref()
            .map(|w| w.events_path().to_path_buf())
    }

    pub fn activity_summary(&self) -> crate::activity::ActivitySummary {
        crate::activity::summarize_events(&self.sink.snapshot_envelopes())
    }

    pub async fn plan_system_prompt(&self) -> Option<String> {
        let store = crate::memory::plan::PlanStore::at(&self.dir);
        let plan = store.latest().await.ok().flatten()?;
        Some(crate::tools::plan::render_plan(&plan))
    }

    pub fn goal(&self) -> Option<String> {
        if let Some(cached) = self.watch.goal.borrow().clone() {
            return Some(cached);
        }
        load_goal(&self.dir)
    }

    pub fn subscribe_goal(&self) -> watch::Receiver<Option<String>> {
        self.watch.goal.subscribe()
    }

    pub fn goal_watch(&self) -> &watch::Sender<Option<String>> {
        &self.watch.goal
    }

    pub fn subscribe_context(&self) -> watch::Receiver<ContextSnapshot> {
        self.watch.context.subscribe()
    }

    pub fn subscribe_attach(&self) -> watch::Receiver<usize> {
        self.watch.attach.subscribe()
    }

    pub fn subscribe_pending_approvals(&self) -> watch::Receiver<Vec<PendingApproval>> {
        self.interactions.approval.subscribe()
    }

    pub fn meta(&self) -> Option<crate::session_meta::SessionMeta> {
        crate::session_meta::SessionMeta::load(&self.dir)
    }

    pub fn request_manual_compact(&self) {
        self.context
            .compaction
            .manual_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn take_manual_compact_request(&self) -> bool {
        self.context
            .compaction
            .manual_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_goal(&self, goal: Option<String>) {
        let _ = self.watch.goal.send(goal);
    }

    pub fn set_attach_count(&self, count: usize) {
        let _ = self.watch.attach.send(count);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_llm_call(
        &self,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
        ttft_ms: Option<u64>,
        tokens_per_sec: Option<f64>,
    ) {
        self.record_llm_usage(
            None,
            model,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            ttft_ms,
            tokens_per_sec,
            true,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_context_plan_call(
        &self,
        provider: &str,
        model: &str,
        plan_id: crate::context_plan::ContextPlanId,
        call_purpose: crate::context_plan::ContextCallPurpose,
        call_identity: crate::context_plan::ContextCallIdentity,
        usage: &crate::provider::TokenUsage,
        ttft_ms: Option<u64>,
        tokens_per_sec: Option<f64>,
    ) {
        self.context.record_call(
            provider,
            model,
            call_purpose,
            call_identity.clone(),
            crate::context_plan::ContextUsageRecord {
                plan_id,
                usage: usage.clone(),
            },
        );

        let total_input = usage.prompt_input();
        self.watch.context.send_modify(|snap| {
            let bucket_idx = snap
                .usage_buckets
                .iter()
                .position(|bucket| {
                    bucket.provider == provider
                        && bucket.model == model
                        && bucket.call_purpose == call_purpose
                        && bucket.call_scope == call_identity.scope
                })
                .unwrap_or_else(|| {
                    snap.usage_buckets.push(ContextUsageBucket {
                        provider: provider.to_string(),
                        model: model.to_string(),
                        call_purpose,
                        call_scope: call_identity.scope,
                        ..Default::default()
                    });
                    snap.usage_buckets.len() - 1
                });
            let bucket = &mut snap.usage_buckets[bucket_idx];
            bucket.calls = bucket.calls.saturating_add(1);
            bucket.tokens_in = bucket.tokens_in.saturating_add(total_input);
            bucket.tokens_out = bucket.tokens_out.saturating_add(usage.output);
            bucket.cache_read = bucket.cache_read.saturating_add(usage.cached_input);
            bucket.cache_write = bucket.cache_write.saturating_add(usage.cache_write);
        });

        let updates_model_window = matches!(
            (call_identity.scope, call_purpose),
            (
                crate::context_plan::ContextCallScope::Root,
                crate::context_plan::ContextCallPurpose::General
            )
        );
        self.record_llm_usage(
            Some(provider),
            model,
            total_input,
            usage.output,
            usage.cached_input,
            usage.cache_write,
            ttft_ms,
            tokens_per_sec,
            updates_model_window,
        );
    }

    pub fn last_context_usage(
        &self,
        key: &crate::context_plan::ContextUsageKey,
    ) -> Option<crate::context_plan::ContextUsageRecord> {
        self.context.last_usage(key)
    }

    #[cfg(test)]
    pub(crate) fn context_epoch(&self) -> Option<String> {
        self.context.compaction.context_epoch()
    }

    #[allow(clippy::too_many_arguments)]
    fn record_llm_usage(
        &self,
        provider: Option<&str>,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
        ttft_ms: Option<u64>,
        tokens_per_sec: Option<f64>,
        updates_model_window: bool,
    ) {
        if updates_model_window && tokens_in > 0 {
            self.context
                .compaction
                .model_window_tokens
                .store(tokens_in, std::sync::atomic::Ordering::Relaxed);
        }
        self.watch.context.send_modify(|snap| {
            snap.tokens_in = snap.tokens_in.saturating_add(tokens_in);
            snap.tokens_out = snap.tokens_out.saturating_add(tokens_out);
            snap.cache_read = snap.cache_read.saturating_add(cache_read);
            snap.cache_write = snap.cache_write.saturating_add(cache_write);
            if updates_model_window {
                snap.model = model.to_string();
                if let Some(provider) = provider {
                    snap.provider = provider.to_string();
                }
                snap.last_ttft_ms = ttft_ms.unwrap_or(0);
                snap.last_tokens_per_sec = tokens_per_sec.unwrap_or(0.0);
            }
        });
        if updates_model_window {
            self.refresh_window_snapshot();
        }
    }

    pub fn last_input_tokens(&self) -> u64 {
        self.context
            .compaction
            .model_window_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn acquire_compact_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.context.compaction.lock.lock().await
    }

    pub async fn acquire_compact_lock_owned(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.context.compaction.lock.clone().lock_owned().await
    }

    pub fn compact_lock_handle(&self) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.context.compaction.lock.clone()
    }

    pub fn refresh_window_snapshot(&self) {
        let provider_tokens = self.last_input_tokens();
        let estimated = crate::compaction::estimate_tokens_for_messages(&self.messages());
        let window = if provider_tokens > 0 {
            provider_tokens
        } else {
            estimated
        };
        let model = self.last_model();
        let budget = crate::model_registry::model_info(&model).context_budget;
        self.watch.context.send_modify(|snap| {
            snap.window_tokens = window;
            if budget > 0 {
                snap.window_budget = budget;
            }
        });
        let snap = self.watch.context.borrow();
        PersistedContextState {
            model,
            window_tokens: snap.window_tokens,
            window_budget: snap.window_budget,
        }
        .save(&self.dir);
    }

    pub fn cumulative_input_tokens(&self) -> u64 {
        self.watch.context.borrow().tokens_in
    }

    pub fn reset_input_tokens_to(&self, tokens: u64) {
        self.watch.context.send_modify(|snap| {
            snap.tokens_in = tokens;
        });
    }

    pub fn last_model(&self) -> String {
        self.watch.context.borrow().model.clone()
    }

    pub fn set_current_model(&self, model: impl Into<String>) {
        let model = model.into();
        let budget = crate::model_registry::model_info(&model).context_budget;
        self.watch.context.send_modify(|snap| {
            if snap.model != model {
                snap.provider.clear();
            }
            snap.model = model.clone();
            if budget > 0 {
                snap.window_budget = budget;
            }
        });
        let snap = self.watch.context.borrow();
        PersistedContextState {
            model,
            window_tokens: snap.window_tokens,
            window_budget: snap.window_budget,
        }
        .save(&self.dir);
    }

    pub fn update_mcp_server(&self, status: crate::mcp::McpServerStatus) {
        self.watch.context.send_modify(|snap| {
            if let Some(existing) = snap.mcp_servers.iter_mut().find(|s| s.name == status.name) {
                *existing = status;
            } else {
                snap.mcp_servers.push(status);
            }
        });
    }

    pub fn set_memory_recent_count(&self, count: u16) {
        self.watch.context.send_modify(|snap| {
            snap.memory_recent_count = count;
        });
    }

    pub fn subscribe_todos(&self) -> watch::Receiver<Vec<crate::memory::todo::Todo>> {
        self.watch.todos.subscribe()
    }

    pub fn todos_watch(&self) -> &watch::Sender<Vec<crate::memory::todo::Todo>> {
        &self.watch.todos
    }

    pub fn subscribe_plans(&self) -> watch::Receiver<Vec<crate::memory::plan::Plan>> {
        self.watch.plans.subscribe()
    }

    pub fn plans_watch(&self) -> &watch::Sender<Vec<crate::memory::plan::Plan>> {
        &self.watch.plans
    }

    pub async fn refresh_plans_from_store_async(&self) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        let store = crate::memory::plan::PlanStore::at(&self.dir);
        match store.list().await {
            Ok(list) => {
                let _ = self.watch.plans.send(list);
            }
            Err(e) => {
                crate::notify!(
                    warn,
                    location = Log,
                    stack = dedupe("memory.refresh_plans_async", 60_000),
                    "refresh_plans_from_store_async: {e}"
                );
            }
        }
    }

    pub fn refresh_todos_from_store(&self) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        let store = crate::memory::todo::TodoStore::at(&self.dir);
        match tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current()
                .ok()
                .map(|h| h.block_on(store.list()))
        }) {
            Some(Ok(list)) => {
                let _ = self.watch.todos.send(list);
            }
            Some(Err(e)) => {
                crate::notify!(
                    warn,
                    location = Log,
                    stack = dedupe("memory.refresh_todos", 60_000),
                    "refresh_todos_from_store: {e}"
                );
            }
            None => {}
        }
    }

    pub async fn refresh_todos_from_store_async(&self) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        let store = crate::memory::todo::TodoStore::at(&self.dir);
        match store.list().await {
            Ok(list) => {
                let _ = self.watch.todos.send(list);
            }
            Err(e) => {
                crate::notify!(
                    warn,
                    location = Log,
                    stack = dedupe("memory.refresh_todos_async", 60_000),
                    "refresh_todos_from_store_async: {e}"
                );
            }
        }
    }

    pub fn sink(&self) -> &EventSink {
        &self.sink
    }

    pub fn import_image_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<crate::message::ImageSource, crate::error::RuntimeError> {
        crate::attachment_store::AttachmentStore::at(&self.dir).import_path(path)
    }

    pub fn import_image_bytes(
        &self,
        bytes: &[u8],
        name: Option<&str>,
    ) -> Result<crate::message::ImageSource, crate::error::RuntimeError> {
        crate::attachment_store::AttachmentStore::at(&self.dir).import_bytes(bytes, name)
    }

    pub fn import_image_base64(
        &self,
        data: &str,
        name: Option<&str>,
    ) -> Result<crate::message::ImageSource, crate::error::RuntimeError> {
        crate::attachment_store::AttachmentStore::at(&self.dir).import_base64(data, name)
    }

    pub fn queue_image_bytes(
        &self,
        bytes: &[u8],
        name: Option<&str>,
    ) -> Result<usize, crate::error::RuntimeError> {
        let source = self.import_image_bytes(bytes, name)?;
        Ok(self.queue_image_source(source))
    }

    pub fn queue_image_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<usize, crate::error::RuntimeError> {
        let source = self.import_image_path(path)?;
        Ok(self.queue_image_source(source))
    }

    pub fn queue_image_base64(
        &self,
        data: &str,
        name: Option<&str>,
    ) -> Result<usize, crate::error::RuntimeError> {
        let source = self.import_image_base64(data, name)?;
        Ok(self.queue_image_source(source))
    }

    pub fn queue_image_source(&self, source: crate::message::ImageSource) -> usize {
        let mut pending = self.pending_images.lock().unwrap();
        pending.push(source);
        let count = pending.len();
        let _ = self.watch.attach.send(count);
        count
    }

    pub fn pop_pending_image(&self) -> Option<crate::message::ImageSource> {
        let mut pending = self.pending_images.lock().unwrap();
        let removed = pending.pop();
        let _ = self.watch.attach.send(pending.len());
        removed
    }

    pub fn remove_pending_image(&self, source: &crate::message::ImageSource) -> bool {
        let mut pending = self.pending_images.lock().unwrap();
        let Some(index) = pending.iter().position(|candidate| candidate == source) else {
            return false;
        };
        pending.remove(index);
        let _ = self.watch.attach.send(pending.len());
        true
    }

    pub fn take_pending_images(&self) -> Vec<crate::message::ImageSource> {
        let images = std::mem::take(&mut *self.pending_images.lock().unwrap());
        let _ = self.watch.attach.send(0);
        images
    }

    pub fn restore_pending_images(&self, mut images: Vec<crate::message::ImageSource>) -> usize {
        let mut pending = self.pending_images.lock().unwrap();
        images.append(&mut pending);
        *pending = images;
        let count = pending.len();
        let _ = self.watch.attach.send(count);
        count
    }

    pub fn clear_pending_images(&self) {
        self.pending_images.lock().unwrap().clear();
        let _ = self.watch.attach.send(0);
    }

    pub fn pending_image_names(&self) -> Vec<String> {
        self.pending_images
            .lock()
            .unwrap()
            .iter()
            .map(crate::attachment_store::display_name)
            .collect()
    }

    pub fn pending_images(&self) -> Vec<crate::message::ImageSource> {
        self.pending_images.lock().unwrap().clone()
    }

    pub fn pending_image_count(&self) -> usize {
        self.pending_images.lock().unwrap().len()
    }

    /// Single-writer append. Emits the matching event before the in-memory push
    /// so events.jsonl remains the authority (§I5).
    pub fn append_message(&self, msg: Message, flow_run_id: Option<FlowRunId>) {
        AppendMessageCommand { msg, flow_run_id }.execute(self);
    }

    pub fn append_context_records(
        &self,
        turn_id: TurnId,
        specs: impl IntoIterator<Item = crate::context_plan::ContextRecordSpec>,
    ) -> Vec<crate::context_plan::ContextRecord> {
        let mut messages = self.context.messages.lock().unwrap();
        let records = crate::context_plan::compile_context_records(&messages, specs);
        for record in &records {
            AppendMessageCommand {
                msg: Message::context_record(turn_id.clone(), record.clone()),
                flow_run_id: None,
            }
            .execute_with_messages(self, &mut messages);
        }
        records
    }

    pub fn emit_attachment_degrade(
        &self,
        message_seq: u64,
        part_index: usize,
        file_basename: String,
        reason: String,
    ) {
        self.sink.emit(Event::AttachmentDegraded {
            turn_id: None,
            flow_run_id: None,
            patch: crate::message::AttachmentPatch {
                target: crate::message::AttachmentTarget::Legacy {
                    message_seq,
                    part_index,
                },
                file_basename,
                reason,
            },
        });
    }

    pub fn record_attachment_degrade(&self, reason: &str) -> usize {
        let target = self.last_image_user_msg.lock().unwrap().take();
        let Some(entry) = target else {
            return 0;
        };
        for (part_index, basename) in &entry.images {
            self.sink.emit(Event::AttachmentDegraded {
                turn_id: Some(entry.message_turn_id.clone()),
                flow_run_id: None,
                patch: crate::message::AttachmentPatch {
                    target: crate::message::AttachmentTarget::Legacy {
                        message_seq: entry.message_seq,
                        part_index: *part_index,
                    },
                    file_basename: basename.clone(),
                    reason: reason.into(),
                },
            });
        }
        if let Ok(mut messages) = self.context.messages.lock()
            && let Some(message) = messages.iter_mut().find(|message| {
                message.role == MessageRole::User && message.turn_id == entry.message_turn_id
            })
        {
            for (part_index, basename) in &entry.images {
                if let Some(part) = message.parts.get_mut(*part_index)
                    && matches!(part, crate::message::MessagePart::Image { .. })
                {
                    *part = crate::message::MessagePart::Text {
                        text: format!("[attachment unavailable: {basename} — {reason}]"),
                    };
                }
            }
        }
        entry.images.len()
    }

    pub fn messages(&self) -> crate::message_stream::MessageWindow {
        self.context.messages()
    }

    pub fn messages_full(&self) -> std::sync::Arc<Vec<Message>> {
        self.context.messages_full()
    }

    pub fn context(&self) -> &std::sync::Arc<ContextState> {
        &self.context
    }

    pub fn messages_handle(&self) -> std::sync::Arc<std::sync::Mutex<Vec<Message>>> {
        self.context.messages.clone()
    }

    pub fn message_count(&self) -> usize {
        self.messages().len()
    }

    pub fn user_message_count(&self) -> usize {
        self.messages()
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .count()
    }

    pub fn push_system_note(&self, text: String) {
        let _ = self
            .watch
            .stream_tx
            .send(crate::stream::StreamFrame::Note(text));
    }

    pub fn approval_cooldown_ok_for_compact(&self) -> bool {
        self.sink.last_compact_ago_seconds().is_none_or(|s| s >= 60)
    }

    pub fn emit_compact_warning(
        &self,
        model: &str,
        current_tokens: u64,
        threshold: u64,
        budget: u64,
        reason: &str,
    ) {
        let message = format!(
            "context {current_tokens} > threshold {threshold} (budget {budget}, model {model}); skipping compaction: {reason}"
        );
        self.sink.emit(Event::WatchWarn {
            turn_id: self.current_turn(),
            flow_run_id: None,
            target: "context.compaction".into(),
            trigger: "auto_compact".into(),
            message,
        });
        self.push_system_note(format!("[warn] compaction skipped: {reason}"));
    }

    /// Convenience wrapper that computes the compact range and token count
    /// from the current message window. Used by tests and internal callers
    /// that don't already have a pre-computed range.
    pub fn compact_messages_auto(&self, summary: String) -> Option<CompactResult> {
        let msgs = self.messages();
        let tokens = crate::compaction::estimate_tokens_for_messages(&msgs);
        let info = crate::model_registry::model_info(&self.last_model());
        let target = info.compaction_target_after();
        let range = crate::compaction::find_compact_range(&msgs, target)?;
        self.compact_messages(summary, range, tokens)
    }

    pub fn commit_rewritten_window(
        &self,
        replacement: Vec<Message>,
        before_tokens: u64,
        before_window_tokens: u64,
        rewritten_count: usize,
    ) -> Option<CompactResult> {
        let after_tokens = crate::compaction::estimate_tokens_for_messages(&replacement);
        if rewritten_count == 0 || after_tokens >= before_window_tokens {
            return None;
        }
        let summary = format!(
            "[atman: persistently compacted output from {rewritten_count} retained messages]"
        );
        self.sink.mark_compacted();
        let mut messages = self
            .context
            .messages
            .lock()
            .expect("session messages poisoned");
        self.context.compaction.update_context_epoch(&replacement);
        let mut batch = self.sink.batch();
        batch.emit(Event::ContextCompact {
            session_id: self.id.to_string(),
            flow_run_id: None,
            before_tokens,
            after_tokens,
            compacted_range_start: 0,
            compacted_range_end: 0,
            summary_text: Some(summary.clone()),
            replacement_msg_seq: None,
        });
        batch.emit(Event::CompactionSummary {
            session_id: self.id.to_string(),
            flow_run_id: None,
            range_start: 0,
            range_end: 0,
            compacted_count: rewritten_count,
            before_tokens,
            after_tokens,
            summary: summary.clone(),
        });
        self.context
            .compaction
            .model_window_tokens
            .store(after_tokens, std::sync::atomic::Ordering::Relaxed);
        *messages = replacement.clone();
        batch.emit(Event::Checkpoint {
            session_id: self.id.to_string(),
            flow_run_id: None,
            messages: replacement,
            window_tokens: after_tokens,
        });
        drop(batch);
        drop(messages);
        let _ = self
            .watch
            .stream_tx
            .send(crate::stream::StreamFrame::CompactionSummary {
                phase: crate::stream::CompactionPhase::Finished,
                range_start: 0,
                range_end: 0,
                summary,
                before_tokens,
                after_tokens,
                compacted_count: rewritten_count,
            });
        self.refresh_window_snapshot();
        Some(CompactResult {
            before_tokens,
            after_tokens,
            compacted_start: 0,
            compacted_end: 0,
        })
    }

    pub fn commit_compacted_window(
        &self,
        summary: String,
        replacement: Vec<Message>,
        range: crate::compaction::CompactRange,
        before_tokens: u64,
        before_window_tokens: u64,
    ) -> Option<CompactResult> {
        let after_tokens = crate::compaction::estimate_tokens_for_messages(&replacement);
        if after_tokens >= before_window_tokens {
            self.push_system_note(format!(
                "compaction skipped: replacement would not shrink transcript ({} >= {} tokens)",
                after_tokens, before_window_tokens
            ));
            return None;
        }
        self.sink.mark_compacted();
        let mut messages = self
            .context
            .messages
            .lock()
            .expect("session messages poisoned");
        self.context.compaction.update_context_epoch(&replacement);
        let mut batch = self.sink.batch();
        batch.emit(Event::ContextCompact {
            session_id: self.id.to_string(),
            flow_run_id: None,
            before_tokens,
            after_tokens,
            compacted_range_start: range.start as u64,
            compacted_range_end: range.end.saturating_sub(1) as u64,
            summary_text: Some(summary.clone()),
            replacement_msg_seq: None,
        });
        batch.emit(Event::CompactionSummary {
            session_id: self.id.to_string(),
            flow_run_id: None,
            range_start: range.start as u64,
            range_end: range.end.saturating_sub(1) as u64,
            compacted_count: range.end - range.start,
            before_tokens,
            after_tokens,
            summary: summary.clone(),
        });
        self.context
            .compaction
            .model_window_tokens
            .store(after_tokens, std::sync::atomic::Ordering::Relaxed);
        *messages = replacement.clone();
        batch.emit(Event::Checkpoint {
            session_id: self.id.to_string(),
            flow_run_id: None,
            messages: replacement,
            window_tokens: after_tokens,
        });
        drop(batch);
        drop(messages);
        let _ = self
            .watch
            .stream_tx
            .send(crate::stream::StreamFrame::CompactionSummary {
                phase: crate::stream::CompactionPhase::Finished,
                range_start: range.start,
                range_end: range.end.saturating_sub(1),
                summary,
                before_tokens,
                after_tokens,
                compacted_count: range.end - range.start,
            });
        self.refresh_window_snapshot();
        Some(CompactResult {
            before_tokens,
            after_tokens,
            compacted_start: range.start,
            compacted_end: range.end,
        })
    }

    pub fn compact_messages(
        &self,
        summary: String,
        range: crate::compaction::CompactRange,
        before_tokens: u64,
    ) -> Option<CompactResult> {
        use crate::compaction::{estimate_tokens_for_messages, replace_range_with_summary};
        let msgs = self.messages();
        let turn_id = msgs
            .get(range.start)
            .map(|m| m.turn_id.clone())
            .unwrap_or_else(TurnId::now);
        let after = replace_range_with_summary(&msgs, &range, summary.clone(), turn_id.clone());
        let after_tokens = estimate_tokens_for_messages(&after);
        if after_tokens >= before_tokens {
            self.push_system_note(format!(
                "compaction skipped: summary would not shrink transcript ({} >= {} tokens)",
                after_tokens, before_tokens
            ));
            return None;
        }
        let replacement_msg = after.first().cloned().unwrap_or_else(|| {
            Message::system_compact_summary(
                turn_id.clone(),
                summary.clone(),
                range.start as u64,
                range.end.saturating_sub(1) as u64,
                range.end - range.start,
            )
        });
        self.sink.mark_compacted();
        let mut messages = self
            .context
            .messages
            .lock()
            .expect("session messages poisoned");
        self.context.compaction.update_context_epoch(&after);
        let mut batch = self.sink.batch();
        let replacement_seq = batch
            .emit(Event::SystemMsg {
                turn_id: turn_id.clone(),
                flow_run_id: None,
                message: replacement_msg,
            })
            .seq;
        batch.emit(Event::ContextCompact {
            session_id: self.id.to_string(),
            flow_run_id: None,
            before_tokens,
            after_tokens,
            compacted_range_start: range.start as u64,
            compacted_range_end: range.end.saturating_sub(1) as u64,
            summary_text: Some(summary.clone()),
            replacement_msg_seq: Some(replacement_seq),
        });
        batch.emit(Event::CompactionSummary {
            session_id: self.id.to_string(),
            flow_run_id: None,
            range_start: range.start as u64,
            range_end: range.end.saturating_sub(1) as u64,
            compacted_count: range.end - range.start,
            before_tokens,
            after_tokens,
            summary: summary.clone(),
        });
        self.context
            .compaction
            .model_window_tokens
            .store(after_tokens, std::sync::atomic::Ordering::Relaxed);
        *messages = after.clone();
        batch.emit(Event::Checkpoint {
            session_id: self.id.to_string(),
            flow_run_id: None,
            messages: after,
            window_tokens: after_tokens,
        });
        drop(batch);
        drop(messages);
        let _ = self
            .watch
            .stream_tx
            .send(crate::stream::StreamFrame::CompactionSummary {
                phase: crate::stream::CompactionPhase::Finished,
                range_start: range.start,
                range_end: range.end.saturating_sub(1),
                summary,
                before_tokens,
                after_tokens,
                compacted_count: range.end - range.start,
            });
        self.refresh_window_snapshot();
        Some(CompactResult {
            before_tokens,
            after_tokens,
            compacted_start: range.start,
            compacted_end: range.end,
        })
    }

    pub fn begin_turn(&self, user_msg: Message) -> TurnId {
        BeginTurnCommand {
            user_msg,
            flow_cancel: None,
        }
        .execute(self)
    }

    pub fn begin_turn_with_cancel(
        &self,
        user_msg: Message,
        flow_cancel: CancellationToken,
    ) -> TurnId {
        BeginTurnCommand {
            user_msg,
            flow_cancel: Some(flow_cancel),
        }
        .execute(self)
    }

    pub fn mark_streamed(&self, turn_id: &TurnId) {
        if let Some(turn) = self.turns.lock().unwrap().get_mut(turn_id) {
            turn.streamed = true;
        }
    }

    pub fn take_streamed_flag(&self, turn_id: &TurnId) -> bool {
        self.turns
            .lock()
            .unwrap()
            .get_mut(turn_id)
            .is_some_and(|turn| std::mem::take(&mut turn.streamed))
    }

    pub fn end_turn(&self, turn_id: &TurnId) {
        let mut turns = self.turns.lock().unwrap();
        if turns.remove(turn_id).is_some() {
            self.injection_queue
                .cancel(|injection| injection.turn_id == *turn_id);
            self.sink.emit(Event::TurnEnd {
                turn_id: turn_id.clone(),
            });
            let _ = self
                .stream_tx()
                .send(crate::stream::StreamFrame::TurnEnded {
                    turn_id: turn_id.to_string(),
                });
        }
    }

    /// Returns the sole active turn for embedded clients, never an arbitrary concurrent turn.
    pub fn current_turn(&self) -> Option<TurnId> {
        let turns = self.turns.lock().unwrap();
        if turns.len() == 1 {
            turns.keys().next().cloned()
        } else {
            None
        }
    }

    pub fn enqueue_injection(&self, text: impl Into<String>) -> Result<InjectionId, EnqueueError> {
        self.enqueue_injection_with_level(text, crate::injection::InjectionLevel::L1Nudge, None)
    }

    pub fn enqueue_injection_with_level(
        &self,
        text: impl Into<String>,
        level: crate::injection::InjectionLevel,
        redirect_target: Option<String>,
    ) -> Result<InjectionId, EnqueueError> {
        self.enqueue_injection_for_run(text, level, redirect_target, None)
            .map(|(id, _)| id)
    }

    pub fn enqueue_injection_for_run(
        &self,
        text: impl Into<String>,
        level: crate::injection::InjectionLevel,
        redirect_target: Option<String>,
        target: Option<(&TurnId, FlowRunId)>,
    ) -> Result<(InjectionId, crate::event::EventEnvelope), EnqueueError> {
        let turns = self.turns.lock().unwrap();
        let (turn_id, flow_run_id) = match target {
            Some((turn_id, run_id)) => {
                if !turns.contains_key(turn_id) {
                    return Err(EnqueueError::InactiveTurn(turn_id.clone()));
                }
                (turn_id.clone(), Some(run_id))
            }
            None => match turns.len() {
                0 => return Err(EnqueueError::NoActiveTurn),
                1 => (turns.keys().next().expect("one active turn").clone(), None),
                _ => return Err(EnqueueError::AmbiguousTurn),
            },
        };
        self.flow_registry.with_lifecycle_arbitration(|| {
            let entry = match flow_run_id.as_ref() {
                Some(run_id) => {
                    if matches!(
                        self.flow_registry.execution_state(run_id),
                        Some(crate::flow_authority::FlowExecutionState::Terminal)
                    ) {
                        return Err(EnqueueError::InactiveRun(run_id.clone()));
                    }
                    self.flow_registry.entry_for_run(run_id)
                }
                None => self.flow_registry.lookup("root").ok().filter(|entry| {
                    entry.turn_id == turn_id
                        && !matches!(
                            self.flow_registry.execution_state(&entry.child_run_id),
                            Some(crate::flow_authority::FlowExecutionState::Terminal)
                        )
                }),
            };
            let envelope = if let Some(entry) = entry {
                if entry.turn_id != turn_id {
                    return Err(EnqueueError::InactiveRun(entry.child_run_id.clone()));
                }
                entry
                    .interject(text, level, redirect_target)
                    .map_err(|_| EnqueueError::InactiveRun(entry.child_run_id.clone()))?
                    .expect("session event sink")
            } else {
                let inj = Injection::with_level_for_run(
                    turn_id.clone(),
                    text,
                    level,
                    redirect_target,
                    flow_run_id,
                );
                let envelope = self
                    .injection_queue
                    .enqueue(inj)
                    .expect("session event sink");
                if level == crate::injection::InjectionLevel::L4HardStop {
                    turns
                        .get(&turn_id)
                        .expect("active turn")
                        .flow_cancel
                        .cancel();
                }
                envelope
            };
            let Event::UserInject { injection, .. } = &envelope.event else {
                unreachable!()
            };
            Ok((injection.id.clone(), envelope))
        })
    }

    pub fn subscribe_injections(&self) -> broadcast::Receiver<Injection> {
        self.injection_queue.subscribe()
    }

    /// Atomically consume the highest-priority stop or redirect for this turn.
    /// Corrections and nudges remain pending for an agent call.
    pub fn take_pending_control(&self, turn_id: &TurnId) -> Option<crate::error::RuntimeError> {
        let claim = self.injection_queue.claim_interruption(|inj| {
            inj.turn_id == *turn_id
                && matches!(
                    inj.level,
                    crate::injection::InjectionLevel::L3Redirect
                        | crate::injection::InjectionLevel::L4HardStop
                )
        })?;
        claim.commit(None, || {})?.control_error()
    }

    /// Consume pending nudges and corrections in creation order, preserving controls.
    /// Persists each rendered context message with its consumption state under the compaction lock.
    pub async fn drain_injections(&self, turn_id: &TurnId) -> Vec<Injection> {
        let _compact_guard = self.acquire_compact_lock().await;
        let mut out = Vec::new();
        while let Some(claim) = self
            .injection_queue
            .claim_steering(|inj| inj.turn_id == *turn_id)
        {
            if let Some(injection) = claim.commit(Some(&self.context.messages), || {}) {
                out.push(injection);
            }
        }
        out
    }

    pub(crate) fn injection_queue(&self) -> std::sync::Arc<crate::injection::InjectionQueue> {
        std::sync::Arc::clone(&self.injection_queue)
    }

    pub fn list_pending_injections(&self) -> Vec<Injection> {
        self.injection_queue.pending()
    }

    /// Cancels the sole active turn for embedded clients. Daemon clients target a run token.
    pub fn cancel_flow(&self) {
        let turns = self.turns.lock().unwrap();
        if turns.len() == 1 {
            turns
                .values()
                .next()
                .expect("one active turn")
                .flow_cancel
                .cancel();
        }
    }

    pub fn flow_cancel_token(&self, turn_id: &TurnId) -> Option<CancellationToken> {
        self.turns
            .lock()
            .unwrap()
            .get(turn_id)
            .map(|turn| turn.flow_cancel.clone())
    }

    pub async fn shutdown(&self) {
        let writer = self.writer.lock().unwrap().take();
        if let Some(writer) = writer {
            writer.shutdown().await;
        }
    }

    // Rides FIFO queue ordering: once flush's own barrier is written,
    // every earlier sink.emit is on disk too.
    pub async fn flush_writer(&self) -> Option<crate::event_writer::EventWriterWatermark> {
        let pending = self.writer.lock().unwrap().as_ref()?.request_flush()?;
        pending.await.ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("enqueue_injection called with no active turn")]
    NoActiveTurn,
    #[error("enqueue_injection requires an explicit turn when multiple turns are active")]
    AmbiguousTurn,
    #[error("turn {0} is not active")]
    InactiveTurn(TurnId),
    #[error("flow {0} is not active")]
    InactiveRun(FlowRunId),
}

pub struct AppendMessageCommand {
    pub msg: Message,
    pub flow_run_id: Option<FlowRunId>,
}

impl AppendMessageCommand {
    pub fn execute(&self, session: &Session) -> u64 {
        let mut messages = session.context.messages.lock().unwrap();
        self.execute_with_messages(session, &mut messages)
    }

    fn execute_with_messages(&self, session: &Session, messages: &mut Vec<Message>) -> u64 {
        let flow_run_id_str = self.flow_run_id.as_ref().map(|r| r.0.to_string());
        let mut msg = crate::tools::tool_output::maybe_truncate_tool_message_with_budget(
            &self.msg,
            Some(&session.output_store),
            session.tool_output_budget(),
        );
        msg.ensure_part_ids();
        let is_internal = msg.origin == crate::message::MessageOrigin::Internal;
        let event =
            match msg.role {
                MessageRole::User => Event::UserMsg {
                    turn_id: msg.turn_id.clone(),
                    flow_run_id: self.flow_run_id.clone(),
                    message: msg.clone(),
                },
                MessageRole::Assistant => {
                    if !is_internal {
                        let _ = session.watch.stream_tx.send(
                            crate::stream::StreamFrame::AssistantMsg {
                                flow_run_id: flow_run_id_str.clone(),
                                message: msg.clone(),
                            },
                        );
                        for source in extract_mermaid_blocks(&msg) {
                            let _ = session.watch.stream_tx.send(
                                crate::stream::StreamFrame::MermaidDiagram {
                                    source: source.clone(),
                                },
                            );
                            session
                                .sink
                                .emit(crate::event::Event::MermaidDiagram { source });
                        }
                    }
                    Event::AssistantMsg {
                        turn_id: msg.turn_id.clone(),
                        flow_run_id: self.flow_run_id.clone(),
                        message: msg.clone(),
                    }
                }
                MessageRole::Tool => {
                    if !is_internal {
                        let _ = session.watch.stream_tx.send(
                            crate::stream::StreamFrame::ToolResultMsg {
                                flow_run_id: flow_run_id_str.clone(),
                                message: msg.clone(),
                            },
                        );
                    }
                    Event::ToolResultMsg {
                        turn_id: msg.turn_id.clone(),
                        flow_run_id: self.flow_run_id.clone(),
                        message: msg.clone(),
                    }
                }
                MessageRole::System => Event::SystemMsg {
                    turn_id: msg.turn_id.clone(),
                    flow_run_id: self.flow_run_id.clone(),
                    message: msg.clone(),
                },
            };
        let seq = session.sink.emit_returning_seq(event);
        if matches!(msg.role, MessageRole::User) {
            let images: Vec<(usize, String)> = msg
                .parts
                .iter()
                .enumerate()
                .filter_map(|(i, p)| match p {
                    crate::message::MessagePart::Image { source, .. } => {
                        let basename = match &source.data {
                            crate::message::ImageData::Path { path } => path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            crate::message::ImageData::Base64 { .. } => "base64".into(),
                            crate::message::ImageData::Artifact { .. } => {
                                crate::attachment_store::display_name(source)
                            }
                        };
                        Some((i, basename))
                    }
                    _ => None,
                })
                .collect();
            if !images.is_empty() {
                *session.last_image_user_msg.lock().unwrap() = Some(LastImageUserMsg {
                    message_seq: seq,
                    message_turn_id: msg.turn_id.clone(),
                    images,
                });
            }
        }
        messages.push(msg.clone());
        seq
    }
}

pub struct BeginTurnCommand {
    pub user_msg: Message,
    pub flow_cancel: Option<CancellationToken>,
}

impl BeginTurnCommand {
    pub fn execute(&self, session: &Session) -> TurnId {
        let turn_id = self.user_msg.turn_id.clone();
        let mut turns = session.turns.lock().unwrap();
        assert!(
            !turns.contains_key(&turn_id),
            "turn {turn_id} is already active"
        );
        turns.insert(
            turn_id.clone(),
            TurnState {
                flow_cancel: self.flow_cancel.clone().unwrap_or_default(),
                streamed: false,
            },
        );
        session.sink.emit(Event::TurnStart {
            turn_id: turn_id.clone(),
        });
        let _ = session
            .stream_tx()
            .send(crate::stream::StreamFrame::TurnStarted {
                turn_id: turn_id.to_string(),
            });
        AppendMessageCommand {
            msg: self.user_msg.clone(),
            flow_run_id: None,
        }
        .execute(session);
        turn_id
    }
}

fn extract_mermaid_blocks(msg: &crate::message::Message) -> Vec<String> {
    let text = msg.text_concat();
    let mut blocks = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            let lang = trimmed.trim_start_matches("```").trim();
            if lang == "mermaid" {
                let mut source = String::new();
                for inner in lines.by_ref() {
                    if inner.trim() == "```" {
                        break;
                    }
                    if !source.is_empty() {
                        source.push('\n');
                    }
                    source.push_str(inner);
                }
                if !source.is_empty() {
                    blocks.push(source);
                }
            } else {
                for inner in lines.by_ref() {
                    if inner.trim() == "```" {
                        break;
                    }
                }
            }
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    fn permission_authority() -> crate::flow_authority::EffectiveAuthority {
        use crate::trust::{ExecutionPolicy, PolicyAction, RiskKind};
        crate::flow_authority::EffectiveAuthority {
            execution_policy: ExecutionPolicy::Controlled,
            allowed_tiers: [true; 5],
            allowed_risks: BTreeSet::from([
                RiskKind::Network,
                RiskKind::WorkspaceExternal,
                RiskKind::Irreversible,
                RiskKind::FilesystemWrite,
                RiskKind::ProcessSpawn,
                RiskKind::RepositoryMutation,
            ]),
            tier_ceiling: [PolicyAction::Auto; 5],
            risk_ceiling: [PolicyAction::Auto; 6],
            shell: true,
            permission_management: true,
            workspace_root: None,
        }
    }

    fn submit_session_permission(session: &Session) -> crate::permission::PermissionRequestId {
        let identity = session
            .flow_registry
            .register_root(
                session.id().to_string(),
                crate::event::FlowRunId::now(),
                permission_authority(),
            )
            .unwrap();
        let intent = crate::permission::PermissionIntent {
            tool_use_id: "session-constructor-call".into(),
            tool_name: "bash.spawn".into(),
            call_intent: None,
            tier: crate::tool::Tier::Two,
            risks: BTreeSet::new(),
            args_digest: "sha256:session-constructor".into(),
            preview: None,
            provenance: crate::permission::ResourceProvenance::none(),
        };
        let policy = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Steady,
            ..crate::trust::TrustConfig::default()
        };
        let crate::permission::SubmissionOutcome::Pending(pending) = session
            .permission_broker
            .submit(
                Some(&identity.session_id),
                Some(&identity.run_id),
                intent,
                false,
                &policy,
            )
            .unwrap()
        else {
            panic!("expected pending session permission");
        };
        pending.request.request_id.clone()
    }

    fn write_events(dir: &Path, lines: &[&str]) {
        let path = dir.join("events.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    }

    #[tokio::test]
    async fn resumed_session_parses_each_event_once_and_hands_off_transcript() {
        let root = TempDir::new().unwrap();
        let created = Session::open(root.path()).unwrap();
        let sid = created.id().to_string();
        let turn_id = crate::event::TurnId::now();
        created.sink().emit(crate::event::Event::UserMsg {
            turn_id: turn_id.clone(),
            flow_run_id: None,
            message: crate::message::Message::user_text(turn_id, "once"),
        });
        created.flush_writer().await;
        created.shutdown().await;
        drop(created);
        crate::event_log::reader::reset_parse_attempts();
        let mut transcript = Vec::new();
        let mut observer = |entry| transcript.push(entry);

        let reopened = Session::open_existing_with_replay_observer(
            root.path(),
            &sid,
            None,
            None,
            crate::trust::TrustConfig::default(),
            &mut observer,
        )
        .unwrap();

        assert_eq!(crate::event_log::reader::parse_attempts(), 1);
        assert!(matches!(
            transcript.as_slice(),
            [TranscriptEntry::Message { message, .. }] if message.text_concat() == "once"
        ));
        assert_eq!(reopened.messages_full().len(), 1);
        let turn_id = crate::event::TurnId::now();
        reopened.sink().emit(crate::event::Event::AssistantMsg {
            turn_id: turn_id.clone(),
            flow_run_id: None,
            message: crate::message::Message::assistant_text(turn_id, "after open"),
        });
        let suffix = reopened.transcript_since_open();
        assert!(matches!(
            suffix.as_slice(),
            [TranscriptEntry::Message { message, .. }] if message.text_concat() == "after open"
        ));
        reopened.shutdown().await;
    }

    #[tokio::test]
    async fn restored_session_returns_projection_history_from_the_same_scan() {
        let root = TempDir::new().unwrap();
        let context_id = crate::event::ContextId::now();
        let redactor = std::sync::Arc::new(crate::redact::Redactor::from_pairs(
            &[("test", &format!("history|{context_id}"))],
            crate::redact::RedactMode::Full,
        ));
        let created = Session::open_with_context(root.path(), Some(redactor), None).unwrap();
        let sid = created.id().to_string();
        let unscoped = created.sink().clone();
        let scoped = unscoped.clone().with_context(context_id.clone());
        let sources = [&unscoped, &scoped, &unscoped];
        let emitted = sources.map(|sink| {
            let turn_id = crate::event::TurnId::now();
            sink.emit_returning_envelope(crate::event::Event::UserMsg {
                turn_id: turn_id.clone(),
                flow_run_id: None,
                message: crate::message::Message::user_text(turn_id, "history"),
            })
        });
        created.flush_writer().await;
        created.shutdown().await;
        drop(created);
        crate::event_log::reader::reset_parse_attempts();

        let restored = Session::restore_existing_with_context_and_trust(
            root.path(),
            &sid,
            None,
            None,
            crate::trust::TrustConfig::default(),
        )
        .unwrap();

        assert_eq!(crate::event_log::reader::parse_attempts(), 3);
        assert_eq!(restored.events.len(), 3);
        for (restored, original) in restored.events.iter().zip(&emitted) {
            assert_eq!(restored.seq, original.seq);
            assert_eq!(restored.ts, original.ts);
            assert_eq!(restored.context_id, original.context_id);
            assert!(matches!(
                &restored.event,
                crate::event::Event::UserMsg { message, .. }
                    if message.text_concat() == "<REDACTED:test>"
            ));
        }
        restored.session.shutdown().await;
    }

    #[test]
    #[ignore = "large synthetic verification for the streaming resume path"]
    fn resume_parses_five_hundred_thousand_lines_once() {
        use std::io::Write;

        const EVENT_COUNT: usize = 500_000;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.jsonl");
        let envelope = crate::event::EventEnvelope::new(
            1,
            crate::event::Event::TurnStart {
                turn_id: crate::event::TurnId::now(),
            },
        );
        let mut line = serde_json::to_vec(&envelope).unwrap();
        line.push(b'\n');
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        for _ in 0..EVENT_COUNT {
            writer.write_all(&line).unwrap();
        }
        writer.flush().unwrap();
        let file_bytes = std::fs::metadata(&path).unwrap().len();

        crate::event_log::reader::reset_parse_attempts();
        let started = std::time::Instant::now();
        let mut transcript = Vec::new();
        let mut observer = |entry| transcript.push(entry);
        let _ =
            crate::event_log::replay::SessionReplay::from_path(&path, Some(&mut observer)).unwrap();
        let elapsed = started.elapsed();
        let attempts = crate::event_log::reader::parse_attempts();
        assert_eq!(attempts, EVENT_COUNT as u64);
        eprintln!(
            "baseline resume: events={EVENT_COUNT} file_bytes={file_bytes} parses={attempts} elapsed_ms={}",
            elapsed.as_millis()
        );
    }

    #[test]
    fn new_session_constructor_persists_supplied_trust() {
        let root = TempDir::new().unwrap();
        let trust = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Eager,
            ..crate::trust::TrustConfig::default()
        };
        let session = Session::open_with_trust(root.path(), trust.clone()).unwrap();
        assert_eq!(session.trust_config(), trust);
        assert_eq!(read_trust(session.dir()).unwrap(), trust);
    }

    #[test]
    fn existing_session_constructor_preserves_session_trust_over_global() {
        let root = TempDir::new().unwrap();
        let session_trust = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Eager,
            ..crate::trust::TrustConfig::default()
        };
        let created = Session::open_with_trust(root.path(), session_trust.clone()).unwrap();
        let sid = created.id().to_string();
        drop(created);
        let global_trust = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Reckless,
            ..crate::trust::TrustConfig::default()
        };
        let reopened = Session::open_existing_with_trust(root.path(), &sid, global_trust).unwrap();
        assert_eq!(reopened.trust_config(), session_trust);
    }

    #[test]
    fn missing_trust_uses_global_and_persists_it() {
        let root = TempDir::new().unwrap();
        let created = Session::open(root.path()).unwrap();
        let sid = created.id().to_string();
        let session_dir = created.dir().to_path_buf();
        drop(created);
        std::fs::remove_file(trust_path(&session_dir)).unwrap();
        let global_trust = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Reckless,
            ..crate::trust::TrustConfig::default()
        };
        let reopened =
            Session::open_existing_with_trust(root.path(), &sid, global_trust.clone()).unwrap();
        assert_eq!(reopened.trust_config(), global_trust);
        assert_eq!(read_trust(&session_dir).unwrap(), global_trust);
    }

    #[test]
    fn corrupt_trust_rejects_existing_session() {
        let root = TempDir::new().unwrap();
        let created = Session::open(root.path()).unwrap();
        let sid = created.id().to_string();
        let session_dir = created.dir().to_path_buf();
        drop(created);
        std::fs::write(trust_path(&session_dir), b"not json").unwrap();
        let error = match Session::open_existing_with_trust(
            root.path(),
            &sid,
            crate::trust::TrustConfig::default(),
        ) {
            Ok(_) => panic!("corrupt trust snapshot was accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, SessionOpenError::Trust { .. }));
    }

    #[test]
    fn open_existing_rejects_obsolete_outside_in_trust_snapshot() {
        let root = TempDir::new().unwrap();
        let created = Session::open(root.path()).unwrap();
        let sid = created.id().to_string();
        let session_dir = created.dir().to_path_buf();
        drop(created);
        std::fs::write(
            trust_path(&session_dir),
            br#"{"mode":"steady","outside":"allow"}"#,
        )
        .unwrap();

        let error = match Session::open_existing_with_trust(
            root.path(),
            &sid,
            crate::trust::TrustConfig::default(),
        ) {
            Ok(_) => panic!("obsolete outside field was accepted"),
            Err(error) => error,
        };

        assert!(
            matches!(error, SessionOpenError::Trust { source, .. } if source.to_string().contains("outside"))
        );
    }

    #[test]
    fn open_existing_rejects_obsolete_nested_risk_in_trust_snapshot() {
        let root = TempDir::new().unwrap();
        let created = Session::open(root.path()).unwrap();
        let sid = created.id().to_string();
        let session_dir = created.dir().to_path_buf();
        drop(created);
        std::fs::write(
            trust_path(&session_dir),
            br#"{"mode":"eager","risks":{"eager":{"outside_workspace":"deny"}}}"#,
        )
        .unwrap();
        let valid = Session::open_existing_with_trust(
            root.path(),
            &sid,
            crate::trust::TrustConfig::default(),
        )
        .unwrap();
        assert_eq!(
            valid
                .trust_config()
                .resolve_risk(crate::trust::RiskKind::WorkspaceExternal),
            crate::trust::PolicyAction::Deny
        );
        drop(valid);

        std::fs::write(
            trust_path(&session_dir),
            br#"{"mode":"eager","risks":{"eager":{"sandbox_violation":"deny","outside_workspace":"deny"}}}"#,
        )
        .unwrap();
        let error = match Session::open_existing_with_trust(
            root.path(),
            &sid,
            crate::trust::TrustConfig::default(),
        ) {
            Ok(_) => panic!("obsolete sandbox_violation risk was accepted"),
            Err(error) => error,
        };

        assert!(
            matches!(error, SessionOpenError::Trust { source, .. } if source.to_string().contains("sandbox_violation"))
        );
    }

    #[test]
    fn session_permission_pipeline_a_b_binds_each_broker_to_only_its_registry() {
        let root = TempDir::new().unwrap();
        let session_a = Session::open(root.path()).unwrap();
        let session_b = Session::open(root.path()).unwrap();
        assert!(
            session_a
                .permission_broker
                .is_for_registry(&session_a.flow_registry)
        );
        assert!(
            session_b
                .permission_broker
                .is_for_registry(&session_b.flow_registry)
        );
        assert!(
            !session_a
                .permission_broker
                .is_for_registry(&session_b.flow_registry)
        );
        assert!(
            !session_b
                .permission_broker
                .is_for_registry(&session_a.flow_registry)
        );
    }

    #[tokio::test]
    async fn fresh_persistent_session_persists_permission_before_matching_stream_frame() {
        let root = TempDir::new().unwrap();
        let session = Session::open(root.path()).unwrap();
        let mut stream = session.stream_subscribe();

        let request_id = submit_session_permission(&session);

        assert!(session.sink().snapshot().iter().any(|event| matches!(
            event,
            crate::event::Event::PermissionRequestCreated { payload }
                if payload.request_id.as_ref() == Some(&request_id)
        )));
        session.flush_writer().await;
        let persisted = std::fs::read_to_string(session.dir().join("events.jsonl")).unwrap();
        let request_id_text = request_id.to_string();
        assert!(persisted.lines().any(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["type"] == "permission_request_created"
                && value["payload"]["request_id"].as_str() == Some(request_id_text.as_str())
        }));
        assert!(matches!(
            stream.try_recv().unwrap(),
            StreamFrame::PermissionRequestCreated { payload, .. }
                if payload.request_id.as_ref() == Some(&request_id)
        ));
        session.shutdown().await;
    }

    #[tokio::test]
    async fn reopened_and_ephemeral_sessions_install_permission_projectors() {
        let root = TempDir::new().unwrap();
        let created = Session::open(root.path()).unwrap();
        let sid = created.id().to_string();
        created.shutdown().await;
        drop(created);

        let reopened = Session::open_existing(root.path(), &sid).unwrap();
        let mut reopened_stream = reopened.stream_subscribe();
        let reopened_id = submit_session_permission(&reopened);
        assert!(reopened.sink().snapshot().iter().any(|event| matches!(
            event,
            crate::event::Event::PermissionRequestCreated { payload }
                if payload.request_id.as_ref() == Some(&reopened_id)
        )));
        assert!(matches!(
            reopened_stream.try_recv().unwrap(),
            StreamFrame::PermissionRequestCreated { payload, .. }
                if payload.request_id.as_ref() == Some(&reopened_id)
        ));
        reopened.shutdown().await;

        let ephemeral = Session::open_ephemeral();
        let mut ephemeral_stream = ephemeral.stream_subscribe();
        let ephemeral_id = submit_session_permission(&ephemeral);
        assert!(ephemeral.sink().snapshot().iter().any(|event| matches!(
            event,
            crate::event::Event::PermissionRequestCreated { payload }
                if payload.request_id.as_ref() == Some(&ephemeral_id)
        )));
        assert!(matches!(
            ephemeral_stream.try_recv().unwrap(),
            StreamFrame::PermissionRequestCreated { payload, .. }
                if payload.request_id.as_ref() == Some(&ephemeral_id)
        ));
    }

    #[tokio::test]
    async fn reopening_session_does_not_hydrate_permission_broker_state() {
        let root = TempDir::new().unwrap();
        let created = Session::open(root.path()).unwrap();
        let sid = created.id().to_string();
        submit_session_permission(&created);
        created.flush_writer().await;
        created.shutdown().await;
        drop(created);

        let reopened = Session::open_existing(root.path(), &sid).unwrap();
        assert!(reopened.permission_broker.list().is_empty());
        assert!(reopened.permission_broker.grants().is_empty());
        let actor = reopened
            .flow_registry
            .register_root(sid, crate::event::FlowRunId::now(), permission_authority())
            .unwrap();
        assert!(
            reopened
                .permission_broker
                .visible_group_list(&actor)
                .unwrap()
                .is_empty()
        );
        reopened.shutdown().await;
    }

    #[test]
    fn update_trust_persists_then_notifies_subscribers() {
        let root = TempDir::new().unwrap();
        let session =
            Session::open_with_trust(root.path(), crate::trust::TrustConfig::default()).unwrap();
        let mut rx = session.subscribe_trust();
        let mut next = session.trust_config();
        next.mode = crate::trust::TrustMode::Eager;

        session.update_trust(next.clone(), |_| Ok(())).unwrap();

        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), next);
        assert_eq!(read_trust(&session.dir).unwrap(), next);
    }

    #[test]
    fn update_trust_rolls_back_session_when_global_persist_fails() {
        let root = TempDir::new().unwrap();
        let previous = crate::trust::TrustConfig::default();
        let session = Session::open_with_trust(root.path(), previous.clone()).unwrap();
        let rx = session.subscribe_trust();
        let mut next = previous.clone();
        next.mode = crate::trust::TrustMode::Reckless;

        let error = session
            .update_trust(next, |_| Err(std::io::Error::other("global write failed")))
            .unwrap_err();

        assert!(
            matches!(error, TrustUpdateError::Global(source) if source.to_string() == "global write failed")
        );
        assert!(!rx.has_changed().unwrap());
        assert_eq!(session.trust_config(), previous);
        assert_eq!(read_trust(&session.dir).unwrap(), previous);
    }

    #[test]
    fn update_trust_reports_rollback_failure() {
        let root = TempDir::new().unwrap();
        let previous = crate::trust::TrustConfig::default();
        let session = Session::open_with_trust(root.path(), previous.clone()).unwrap();
        let trust_file = trust_path(session.dir());
        let mut next = previous;
        next.mode = crate::trust::TrustMode::Eager;

        let error = session
            .update_trust(next, |_| {
                std::fs::remove_file(&trust_file).unwrap();
                std::fs::create_dir(&trust_file).unwrap();
                Err(std::io::Error::other("global write failed"))
            })
            .unwrap_err();

        assert!(matches!(error, TrustUpdateError::RollbackFailed { .. }));
    }

    #[test]
    fn successful_flow_count_triggers_at_powers_of_three_and_resets_per_session() {
        let session = Session::open_ephemeral();
        let mut hits = Vec::new();
        for _ in 0..27 {
            if let Some(count) = session.record_successful_flow() {
                hits.push(count);
            }
        }
        assert_eq!(hits, vec![3, 9, 27]);
        assert_eq!(session.successful_flow_count(), 27);
        assert_eq!(Session::open_ephemeral().successful_flow_count(), 0);
    }

    #[test]
    fn commit_rewritten_window_uses_checkpoint_without_legacy_range_replay() {
        let session = Session::open_ephemeral();
        assert_eq!(session.context_epoch(), None);
        let original = vec![
            Message::user_text(TurnId::now(), "first user"),
            Message::assistant_text(TurnId::now(), "large output".repeat(2_000)),
            Message::user_text(TurnId::now(), "current user"),
        ];
        for message in original.clone() {
            session.append_message(message, None);
        }
        let replacement = vec![
            original[0].clone(),
            Message::assistant_text(TurnId::now(), "persisted omission"),
            original[2].clone(),
        ];
        let before_tokens = crate::compaction::estimate_tokens_for_messages(&original);

        session
            .commit_rewritten_window(replacement.clone(), before_tokens, before_tokens, 1)
            .expect("rewrite commit");

        assert_eq!(
            session.context_epoch(),
            Some(checkpoint_epoch_digest(&replacement))
        );
        assert_eq!(session.messages().as_ref(), replacement.as_slice());
        let events = session.sink().snapshot();
        assert!(events.iter().any(|event| matches!(
            event,
            Event::ContextCompact {
                replacement_msg_seq: None,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::SystemMsg { .. }))
        );
        let checkpoint = events
            .into_iter()
            .find(|event| matches!(event, Event::Checkpoint { .. }))
            .expect("checkpoint");
        let replay = crate::message_stream::MessageStream::new(std::sync::Arc::new(
            std::sync::Mutex::new(vec![crate::event::EventEnvelope::new(1, checkpoint)]),
        ));
        assert_eq!(&*replay.window(), replacement.as_slice());
    }

    #[tokio::test]
    async fn restored_epoch_uses_the_checkpoint_fact_instead_of_remaining_message_positions() {
        for checkpoint in [
            Vec::new(),
            vec![
                Message::user_text(TurnId::now(), "retained user"),
                Message::assistant_text(TurnId::now(), "retained answer"),
            ],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let session = Session::open(dir.path()).unwrap();
            session.append_message(Message::user_text(TurnId::now(), "x".repeat(40_000)), None);
            session
                .commit_rewritten_window(checkpoint.clone(), 10_000, 10_000, 1)
                .unwrap();
            let expected = Some(checkpoint_epoch_digest(&checkpoint));
            assert_eq!(session.context_epoch(), expected);
            if !checkpoint.is_empty() {
                let summary =
                    Message::system_compact_summary(TurnId::now(), "later summary", 0, 0, 1);
                let summary_seq = AppendMessageCommand {
                    msg: summary,
                    flow_run_id: None,
                }
                .execute(&session);
                session.sink().emit(Event::ContextCompact {
                    session_id: session.id().to_string(),
                    flow_run_id: None,
                    before_tokens: 100,
                    after_tokens: 10,
                    compacted_range_start: 0,
                    compacted_range_end: 0,
                    summary_text: None,
                    replacement_msg_seq: Some(summary_seq),
                });
            }
            let child = FlowRunId::now();
            session.sink().emit(Event::FlowStart {
                run_id: child.clone(),
                turn_id: None,
                flow_name: "child".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: true,
            });
            session.sink().emit(Event::Checkpoint {
                session_id: session.id().to_string(),
                flow_run_id: Some(child),
                messages: vec![Message::user_text(TurnId::now(), "unrelated child")],
                window_tokens: 10,
            });
            session.append_message(Message::user_text(TurnId::now(), "later user"), None);
            let window = session.messages().to_vec();
            assert_ne!(window, checkpoint);
            assert_eq!(session.context_epoch(), expected);
            session.flush_writer().await.unwrap();
            let restored = Session::open_existing(dir.path(), &session.id().to_string()).unwrap();
            assert_eq!(restored.messages().to_vec(), window);
            assert_eq!(restored.context_epoch(), expected);
            session.shutdown().await;
            restored.shutdown().await;
        }
    }

    #[test]
    fn commit_compacted_window_updates_live_handle_and_checkpoint_replay() {
        let session = Session::open_ephemeral();
        let old = vec![
            Message::user_text(TurnId::now(), "old user".repeat(2_000)),
            Message::assistant_text(TurnId::now(), "old assistant".repeat(2_000)),
            Message::user_text(TurnId::now(), "current user"),
        ];
        for message in old.clone() {
            session.append_message(message, None);
        }
        let replacement = vec![
            Message::system_compact_summary(TurnId::now(), "anchor", 0, 1, 2),
            old[2].clone(),
            Message::assistant_text(TurnId::now(), "persisted omission"),
        ];
        let before_tokens = crate::compaction::estimate_tokens_for_messages(&old);
        let range = crate::compaction::CompactRange {
            start: 0,
            end: 2,
            tokens_saved_estimate: before_tokens,
        };

        session
            .commit_compacted_window(
                "anchor".into(),
                replacement.clone(),
                range,
                before_tokens,
                before_tokens,
            )
            .expect("commit");

        assert_eq!(session.messages().as_ref(), replacement.as_slice());
        assert_eq!(
            session.messages_handle().lock().unwrap().as_slice(),
            replacement.as_slice()
        );
        let checkpoint = session
            .sink()
            .snapshot()
            .into_iter()
            .find(|event| matches!(event, Event::Checkpoint { .. }))
            .expect("checkpoint");
        let replay = crate::message_stream::MessageStream::new(std::sync::Arc::new(
            std::sync::Mutex::new(vec![crate::event::EventEnvelope::new(1, checkpoint)]),
        ));
        assert_eq!(&*replay.window(), replacement.as_slice());
    }

    #[test]
    fn replay_applies_attachment_degraded_patch() {
        let dir = TempDir::new().unwrap();
        let user_msg = r#"{"type":"user_msg","seq":5,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/photo.png"}}},{"type":"text","text":"describe"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        let degrade = r#"{"type":"attachment_degraded","seq":6,"turn_id":null,"flow_run_id":null,"message_seq":5,"part_index":0,"file_basename":"photo.png","reason":"image_too_large","ts":"2026-07-07T00:00:01Z"}"#;
        write_events(dir.path(), &[user_msg, degrade]);
        let entries = replay_transcript_from(&dir.path().join("events.jsonl")).unwrap();
        let msg = entries
            .into_iter()
            .find_map(|e| match e {
                TranscriptEntry::Message { message, .. } => Some(message),
                _ => None,
            })
            .unwrap();
        assert_eq!(msg.parts.len(), 2);
        match &msg.parts[0] {
            crate::message::MessagePart::Text { text } => {
                assert!(text.contains("photo.png"), "expected basename: {text}");
                assert!(text.contains("image_too_large"), "expected reason: {text}");
                assert!(text.starts_with("[attachment unavailable"));
            }
            other => panic!("expected Text stub, got {other:?}"),
        }
        assert!(matches!(
            msg.parts[1],
            crate::message::MessagePart::Text { .. }
        ));
    }

    #[test]
    fn approval_registry_always_queues_for_manual_decision() {
        let reg = std::sync::Arc::new(ApprovalRegistry::new());
        let pending = PendingApproval {
            tool_use_id: "tu42".into(),
            tool_name: "fs.write".into(),
            args_preview: "{}".into(),
            preview: None,
            level: crate::tool::ApprovalLevel::Approve,
            run_id: FlowRunId::now(),
            emitted_at: chrono::Utc::now(),
        };
        let mut rx = reg.request(pending);
        assert_eq!(reg.list_pending().len(), 1);
        assert!(rx.try_recv().is_err(), "should still be queued");
        assert!(reg.decide("tu42", ApprovalDecision::Approve));
        let got = rx.blocking_recv().unwrap();
        assert!(matches!(got, ApprovalDecision::Approve));
        assert!(reg.list_pending().is_empty());
    }

    #[test]
    fn approval_registry_decide_all_flushes_queue() {
        let reg = ApprovalRegistry::new();
        let mut rxs = Vec::new();
        for i in 0..3 {
            rxs.push(reg.request(PendingApproval {
                tool_use_id: format!("tu{i}"),
                tool_name: "bash.exec".into(),
                args_preview: "{}".into(),
                preview: None,
                level: crate::tool::ApprovalLevel::Dangerous,
                run_id: FlowRunId::now(),
                emitted_at: chrono::Utc::now(),
            }));
        }
        assert_eq!(reg.list_pending().len(), 3);
        assert_eq!(
            reg.decide_all(ApprovalDecision::Deny {
                reason: "user cancelled".into()
            }),
            3
        );
        assert!(reg.list_pending().is_empty());
    }

    #[test]
    fn compact_review_registry_auto_accepts_when_no_subscriber() {
        let reg = CompactReviewRegistry::new();
        let pending = PendingCompactReview {
            review_id: "r1".into(),
            summary: "gist".into(),
            slice_preview: String::new(),
            slice_count: 0,
            range_start: 0,
            range_end: 0,
            tokens_before: 0,
            emitted_at: chrono::Utc::now(),
        };
        let rx = reg.request(pending);
        let got = rx.blocking_recv().unwrap();
        assert!(matches!(got, CompactReviewDecision::AcceptAsIs));
        assert!(reg.list_pending().is_none());
    }

    #[test]
    fn compact_review_registry_holds_pending_and_decides() {
        let reg = std::sync::Arc::new(CompactReviewRegistry::new());
        let _sub = reg.subscribe();
        let pending = PendingCompactReview {
            review_id: "r2".into(),
            summary: "old".into(),
            slice_preview: "slice".into(),
            slice_count: 3,
            range_start: 1,
            range_end: 4,
            tokens_before: 500,
            emitted_at: chrono::Utc::now(),
        };
        let mut rx = reg.request(pending);
        assert!(rx.try_recv().is_err(), "should be queued");
        assert!(reg.list_pending().is_some());
        assert!(reg.decide(
            "r2",
            CompactReviewDecision::AcceptEdited {
                summary: "new".into()
            }
        ));
        let got = rx.blocking_recv().unwrap();
        match got {
            CompactReviewDecision::AcceptEdited { summary } => assert_eq!(summary, "new"),
            other => panic!("unexpected decision: {other:?}"),
        }
        assert!(reg.list_pending().is_none());
    }

    #[test]
    fn session_compact_review_registry_emits_ordered_durable_events() {
        let session = Session::open_ephemeral();
        let _sub = session.compact_reviews().subscribe();
        let response = session.compact_reviews().request(PendingCompactReview {
            review_id: "durable-review".into(),
            summary: "summary".into(),
            slice_preview: "preview".into(),
            slice_count: 2,
            range_start: 1,
            range_end: 3,
            tokens_before: 100,
            emitted_at: chrono::Utc::now(),
        });
        let commit = session
            .compact_reviews()
            .decide_with_commit("durable-review", CompactReviewDecision::AcceptAsIs)
            .unwrap();

        assert_eq!(commit.event.unwrap().seq, 2);
        assert!(matches!(
            response.blocking_recv().unwrap(),
            CompactReviewDecision::AcceptAsIs
        ));
        let events = session.sink().snapshot_envelopes();
        assert!(matches!(
            &events[0].event,
            crate::event::Event::CompactReviewRequested { review }
                if review.review_id == "durable-review"
        ));
        assert!(matches!(
            &events[1].event,
            crate::event::Event::CompactReviewResolved {
                review_id,
                abandoned: false,
                ..
            } if review_id == "durable-review"
        ));
    }

    #[test]
    fn compact_review_registry_reject_flushes() {
        let reg = std::sync::Arc::new(CompactReviewRegistry::new());
        let _sub = reg.subscribe();
        let rx = reg.request(PendingCompactReview {
            review_id: "r3".into(),
            summary: String::new(),
            slice_preview: String::new(),
            slice_count: 0,
            range_start: 0,
            range_end: 0,
            tokens_before: 0,
            emitted_at: chrono::Utc::now(),
        });
        assert!(reg.decide("r3", CompactReviewDecision::Reject));
        let got = rx.blocking_recv().unwrap();
        assert!(matches!(got, CompactReviewDecision::Reject));
    }

    #[test]
    fn replay_context_snapshot_accumulates_llm_call_usage() {
        let dir = TempDir::new().unwrap();
        let events = [
            r#"{"type":"llm_call","seq":1,"model":"anthropic/claude-4","provider":"anthropic","usage":{"input":100,"cached_input":10,"output":50,"cache_write":0},"wallclock_ms":1000,"status":{"kind":"ok"},"run_id":"019f0000-0000-7000-0000-000000000099","ts":"2026-07-08T00:00:00Z"}"#,
            r#"{"type":"user_msg","seq":2,"turn_id":"019f0000-0000-7000-0000-000000000002","message":{"role":"user","parts":[{"type":"text","text":"hi"}],"turn_id":"019f0000-0000-7000-0000-000000000002"},"ts":"2026-07-08T00:00:00Z"}"#,
            r#"{"type":"llm_call","seq":3,"model":"anthropic/claude-4","provider":"anthropic","usage":{"input":200,"cached_input":0,"output":80,"cache_write":0},"wallclock_ms":1000,"status":{"kind":"ok"},"run_id":"019f0000-0000-7000-0000-000000000099","ts":"2026-07-08T00:00:01Z"}"#,
        ];
        write_events(dir.path(), &events);
        let snap = replay_context_snapshot_from(&dir.path().join("events.jsonl"));
        assert_eq!(snap.model, "anthropic/claude-4");
        assert_eq!(snap.provider, "anthropic");
        assert_eq!(snap.tokens_in, 310);
        assert_eq!(snap.tokens_out, 130);
        assert_eq!(snap.cache_read, 10);
        assert_eq!(snap.primary_usage().unwrap().tokens_in, 310);
    }

    #[test]
    fn replay_context_snapshot_skips_subagent_llm_calls() {
        let dir = TempDir::new().unwrap();
        let events = [
            r#"{"type":"llm_call","seq":1,"model":"zhipuai/glm-5.2","provider":"zhipu","usage":{"input":100,"cached_input":0,"output":50,"cache_write":0},"wallclock_ms":1000,"status":{"kind":"ok"},"run_id":"019f0000-0000-7000-0000-000000000099","ts":"2026-07-08T00:00:00Z"}"#,
            r#"{"type":"llm_call","seq":2,"model":"gpt-4o-mini","provider":"openai","usage":{"input":200,"cached_input":0,"output":80,"cache_write":0},"wallclock_ms":1000,"status":{"kind":"ok"},"run_id":null,"ts":"2026-07-08T00:00:01Z"}"#,
        ];
        write_events(dir.path(), &events);
        let snap = replay_context_snapshot_from(&dir.path().join("events.jsonl"));
        assert_eq!(snap.model, "zhipuai/glm-5.2");
        assert_eq!(snap.tokens_in, 100);
        assert_eq!(snap.tokens_out, 50);
        assert_eq!(snap.usage_buckets.len(), 1);
    }

    #[test]
    fn replay_context_snapshot_separates_explicit_helper_usage() {
        let dir = TempDir::new().unwrap();
        let events = [
            r#"{"type":"llm_call","seq":1,"model":"primary-model","provider":"primary-provider","context_call_purpose":"general","context_call_identity":{"scope":"root","session_id":"session"},"usage":{"input":20,"cached_input":80,"output":10,"cache_write":50},"wallclock_ms":1000,"ttft_ms":120,"tokens_per_second":20.0,"status":{"kind":"ok"},"run_id":"019f0000-0000-7000-0000-000000000099","ts":"2026-07-08T00:00:00Z"}"#,
            r#"{"type":"llm_call","seq":2,"model":"helper-model","provider":"helper-provider","context_call_purpose":"extraction","context_call_identity":{"scope":"detached"},"usage":{"input":1000,"cached_input":0,"output":80,"cache_write":0},"wallclock_ms":1000,"status":{"kind":"ok"},"run_id":null,"ts":"2026-07-08T00:00:01Z"}"#,
        ];
        write_events(dir.path(), &events);
        let snap = replay_context_snapshot_from(&dir.path().join("events.jsonl"));

        assert_eq!(snap.model, "primary-model");
        assert_eq!(snap.provider, "primary-provider");
        assert_eq!(snap.tokens_in, 1_150);
        assert_eq!(snap.usage_buckets.len(), 2);
        let primary = snap.primary_usage().unwrap();
        assert_eq!(primary.tokens_in, 150);
        assert_eq!(primary.cache_read, 80);
        assert_eq!(snap.last_ttft_ms, 120);
    }

    #[test]
    fn compact_review_mode_parses_all_variants() {
        assert_eq!(
            CompactReviewMode::parse("always"),
            Some(CompactReviewMode::Always)
        );
        assert_eq!(
            CompactReviewMode::parse("manual-only"),
            Some(CompactReviewMode::ManualOnly)
        );
        assert_eq!(
            CompactReviewMode::parse("manual_only"),
            Some(CompactReviewMode::ManualOnly)
        );
        assert_eq!(
            CompactReviewMode::parse("never"),
            Some(CompactReviewMode::Never)
        );
        assert_eq!(CompactReviewMode::parse(" bogus "), None);
    }

    #[test]
    fn compact_review_mode_should_review_matrix() {
        assert!(CompactReviewMode::Always.should_review(false));
        assert!(CompactReviewMode::Always.should_review(true));
        assert!(!CompactReviewMode::ManualOnly.should_review(false));
        assert!(CompactReviewMode::ManualOnly.should_review(true));
        assert!(!CompactReviewMode::Never.should_review(false));
        assert!(!CompactReviewMode::Never.should_review(true));
    }

    #[test]
    fn compact_review_registry_new_request_rejects_previous() {
        let reg = std::sync::Arc::new(CompactReviewRegistry::new());
        let _sub = reg.subscribe();
        let rx_a = reg.request(PendingCompactReview {
            review_id: "rA".into(),
            summary: String::new(),
            slice_preview: String::new(),
            slice_count: 0,
            range_start: 0,
            range_end: 0,
            tokens_before: 0,
            emitted_at: chrono::Utc::now(),
        });
        let _rx_b = reg.request(PendingCompactReview {
            review_id: "rB".into(),
            summary: String::new(),
            slice_preview: String::new(),
            slice_count: 0,
            range_start: 0,
            range_end: 0,
            tokens_before: 0,
            emitted_at: chrono::Utc::now(),
        });
        let got = rx_a.blocking_recv().unwrap();
        assert!(matches!(got, CompactReviewDecision::Reject));
    }

    fn mk_form(form_id: &str, prompt: &str) -> crate::form::PendingForm {
        crate::form::PendingForm {
            form_id: form_id.into(),
            run_id: crate::event::FlowRunId::now(),
            tool_use_id: "tu".into(),
            kind: crate::form::FormKind::Confirm {
                prompt: prompt.into(),
            },
            form: crate::form::CompositeForm {
                questions: vec![crate::form::FormQuestion {
                    id: "question".into(),
                    kind: crate::form::FormKind::Confirm {
                        prompt: prompt.into(),
                    },
                }],
            },
            emitted_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn form_registry_auto_cancels_without_subscriber() {
        let reg = FormRegistry::new();
        let rx = reg.request(mk_form("f1", "sure?"));
        let got = rx.blocking_recv().unwrap();
        assert_eq!(got, crate::form::FormSubmission::Rejected);
        assert!(reg.list_pending().is_empty());
    }

    #[test]
    fn form_registry_delivers_answer_by_form_id() {
        let reg = std::sync::Arc::new(FormRegistry::new());
        let _sub = reg.subscribe();
        let rx = reg.request(mk_form("fA", "?"));
        assert_eq!(reg.list_pending().len(), 1);
        let ok = reg.submit(
            "fA",
            crate::form::FormSubmission::Submitted {
                answers: vec![crate::form::FormAnswer::Confirmed { value: true }],
            },
        );
        assert!(ok);
        let got = rx.blocking_recv().unwrap();
        assert_eq!(
            got,
            crate::form::FormSubmission::Submitted {
                answers: vec![crate::form::FormAnswer::Confirmed { value: true }],
            }
        );
        assert!(reg.list_pending().is_empty());
    }

    #[test]
    fn session_form_registry_emits_ordered_durable_events() {
        let session = Session::open_ephemeral();
        let _sub = session.forms().subscribe();
        let pending = mk_form("durable", "Continue?");
        let run_id = pending.run_id.clone();
        let response = session.forms().request(pending);
        let commit = session
            .forms()
            .submit_with_commit(
                "durable",
                crate::form::FormSubmission::Submitted {
                    answers: vec![crate::form::FormAnswer::Confirmed { value: true }],
                },
            )
            .unwrap()
            .unwrap();

        assert_eq!(commit.event.unwrap().seq, 2);
        assert!(matches!(
            response.blocking_recv().unwrap(),
            crate::form::FormSubmission::Submitted { .. }
        ));
        let events = session.sink().snapshot_envelopes();
        assert!(matches!(
            &events[0].event,
            crate::event::Event::FormRequested { form }
                if form.form_id == "durable" && form.run_id == run_id
        ));
        assert!(matches!(
            &events[1].event,
            crate::event::Event::FormResolved {
                form_id,
                run_id: resolved_run_id,
                abandoned: false,
                ..
            } if form_id == "durable" && resolved_run_id == &run_id
        ));
    }

    #[test]
    fn form_registry_submit_unknown_id_is_noop() {
        let reg = std::sync::Arc::new(FormRegistry::new());
        let _sub = reg.subscribe();
        let _rx = reg.request(mk_form("real", "?"));
        assert!(!reg.submit("ghost", crate::form::FormSubmission::Rejected));
        assert_eq!(reg.list_pending().len(), 1);
    }

    #[test]
    fn form_registry_cancel_removes_one_pending_form() {
        let reg = std::sync::Arc::new(FormRegistry::new());
        let _sub = reg.subscribe();
        let rx = reg.request(mk_form("cancel", "?"));
        assert!(reg.cancel("cancel"));
        assert_eq!(
            rx.blocking_recv().unwrap(),
            crate::form::FormSubmission::Rejected
        );
        assert!(reg.list_pending().is_empty());
    }

    #[test]
    fn form_registry_cancel_all_flushes_pending() {
        let reg = std::sync::Arc::new(FormRegistry::new());
        let _sub = reg.subscribe();
        let rx_a = reg.request(mk_form("a", "?"));
        let rx_b = reg.request(mk_form("b", "?"));
        reg.cancel_all();
        assert_eq!(
            rx_a.blocking_recv().unwrap(),
            crate::form::FormSubmission::Rejected
        );
        assert_eq!(
            rx_b.blocking_recv().unwrap(),
            crate::form::FormSubmission::Rejected
        );
        assert!(reg.list_pending().is_empty());
    }

    #[test]
    fn form_registry_queues_multiple_pending() {
        let reg = std::sync::Arc::new(FormRegistry::new());
        let _sub = reg.subscribe();
        let _rx1 = reg.request(mk_form("1", "?"));
        let _rx2 = reg.request(mk_form("2", "?"));
        let pending = reg.list_pending();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].form_id, "1");
        assert_eq!(pending[1].form_id, "2");
    }

    #[test]
    fn replay_without_degraded_events_preserves_image_parts() {
        let dir = TempDir::new().unwrap();
        let user_msg = r#"{"type":"user_msg","seq":1,"turn_id":"019f0000-0000-7000-0000-000000000002","message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/x.png"}}}],"turn_id":"019f0000-0000-7000-0000-000000000002"},"ts":"2026-07-07T00:00:00Z"}"#;
        write_events(dir.path(), &[user_msg]);
        let entries = replay_transcript_from(&dir.path().join("events.jsonl")).unwrap();
        let msg = entries
            .into_iter()
            .find_map(|e| match e {
                TranscriptEntry::Message { message, .. } => Some(message),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            msg.parts[0],
            crate::message::MessagePart::Image { .. }
        ));
    }

    #[test]
    fn attachment_degrade_updates_only_the_target_user_message() {
        fn image_message(path: &str) -> Message {
            Message {
                role: MessageRole::User,
                parts: vec![crate::message::MessagePart::Image {
                    id: None,
                    source: crate::message::ImageSource {
                        media_type: "image/png".into(),
                        data: crate::message::ImageData::Path { path: path.into() },
                        detail: crate::provider::ImageDetail::Auto,
                    },
                }],
                turn_id: TurnId::now(),
                origin: crate::message::MessageOrigin::User,
            }
        }

        let session = Session::open_ephemeral();
        session.append_message(image_message("/tmp/first.png"), None);
        session.append_message(image_message("/tmp/second.png"), None);

        assert_eq!(session.record_attachment_degrade("invalid_image"), 1);
        let messages = session.messages_handle();
        let messages = messages.lock().unwrap();
        assert!(matches!(
            messages[0].parts[0],
            crate::message::MessagePart::Image { .. }
        ));
        assert!(matches!(
            &messages[1].parts[0],
            crate::message::MessagePart::Text { text } if text.contains("second.png")
        ));
    }

    #[test]
    fn replay_messages_from_old_format_no_seq_no_ts() {
        let dir = TempDir::new().unwrap();
        // Old-style JSONL: no seq, no ts on events
        let user_json = r#"{"type":"user_msg","turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"text","text":"hello"}],"turn_id":"019f0000-0000-7000-0000-000000000001"}}"#;
        let asst_json = r#"{"type":"assistant_msg","turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"assistant","parts":[{"type":"text","text":"hi there"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"flow_run_id":null}"#;
        write_events(dir.path(), &[user_json, asst_json]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert_eq!(msgs.len(), 2, "should load both messages from old format");
        assert_eq!(msgs[0].text_concat(), "hello");
        assert_eq!(msgs[1].text_concat(), "hi there");
    }

    #[test]
    fn replay_messages_from_old_format_with_null_fields() {
        let dir = TempDir::new().unwrap();
        // Old JSON with null turn_id / flow_run_id (graceful parse)
        let sys_json = r#"{"type":"system_msg","turn_id":"019f0000-0000-7000-0000-000000000003","message":{"role":"system","parts":[{"type":"text","text":"note"}],"turn_id":"019f0000-0000-7000-0000-000000000003"}}"#;
        write_events(dir.path(), &[sys_json]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text_concat(), "note");
    }

    #[test]
    fn replay_messages_from_applies_attachment_degrade_event() {
        let dir = TempDir::new().unwrap();
        let user_msg = r#"{"type":"user_msg","seq":5,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/photo.png"}}},{"type":"text","text":"describe"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        let degrade = r#"{"type":"attachment_degraded","seq":6,"turn_id":null,"flow_run_id":null,"message_seq":5,"part_index":0,"file_basename":"photo.png","reason":"image_too_large","ts":"2026-07-07T00:00:01Z"}"#;
        write_events(dir.path(), &[user_msg, degrade]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert_eq!(msgs.len(), 1, "only the user message");
        assert_eq!(msgs[0].parts.len(), 2);
        assert!(matches!(
            &msgs[0].parts[0],
            crate::message::MessagePart::Text { text }
                if text.contains("photo.png") && text.contains("image_too_large")
        ));
        assert!(
            matches!(msgs[0].parts[1], crate::message::MessagePart::Text { .. }),
            "second part should remain text"
        );
    }

    #[test]
    fn replay_messages_from_degrade_before_message_is_noop() {
        let dir = TempDir::new().unwrap();
        // Degrade event appears BEFORE the message it references (should not crash)
        let degrade = r#"{"type":"attachment_degraded","seq":1,"turn_id":null,"flow_run_id":null,"message_seq":99,"part_index":0,"file_basename":"x.png","reason":"test","ts":"2026-07-07T00:00:00Z"}"#;
        let user_msg = r#"{"type":"user_msg","seq":2,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/x.png"}}}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:01Z"}"#;
        write_events(dir.path(), &[degrade, user_msg]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert_eq!(msgs.len(), 1);
        // Image part preserved — degrade referenced unknown message_seq
        assert!(
            matches!(msgs[0].parts[0], crate::message::MessagePart::Image { .. }),
            "image should remain when degrade targets unknown seq"
        );
    }

    #[test]
    fn replay_messages_from_degrade_wrong_seq_leaves_image() {
        let dir = TempDir::new().unwrap();
        let user_msg = r#"{"type":"user_msg","seq":1,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/x.png"}}}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        // Degrade references wrong message_seq (2, but message has seq 1)
        let degrade = r#"{"type":"attachment_degraded","seq":2,"turn_id":null,"flow_run_id":null,"message_seq":2,"part_index":0,"file_basename":"x.png","reason":"test","ts":"2026-07-07T00:00:01Z"}"#;
        write_events(dir.path(), &[user_msg, degrade]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(
            matches!(msgs[0].parts[0], crate::message::MessagePart::Image { .. }),
            "image should remain when degrade targets wrong seq"
        );
    }

    #[test]
    fn replay_messages_from_applies_context_compact() {
        let dir = TempDir::new().unwrap();
        let user1 = r#"{"type":"user_msg","seq":1,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"text","text":"old u1"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        let asst1 = r#"{"type":"assistant_msg","seq":2,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"assistant","parts":[{"type":"text","text":"old a1"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"flow_run_id":null,"ts":"2026-07-07T00:00:01Z"}"#;
        let user2 = r#"{"type":"user_msg","seq":3,"turn_id":"019f0000-0000-7000-0000-000000000002","message":{"role":"user","parts":[{"type":"text","text":"old u2"}],"turn_id":"019f0000-0000-7000-0000-000000000002"},"ts":"2026-07-07T00:00:02Z"}"#;
        // replacement message (compact summary)
        let summary = r#"{"type":"system_msg","seq":4,"turn_id":"019f0000-0000-7000-0000-000000000002","message":{"role":"system","parts":[{"type":"compact_summary","summary":"two messages compacted","seq_start":0,"seq_end":1,"count":2}],"turn_id":"019f0000-0000-7000-0000-000000000002"},"ts":"2026-07-07T00:00:03Z"}"#;
        let compact = r#"{"type":"context_compact","seq":5,"session_id":"sess","before_tokens":200,"after_tokens":50,"compacted_range_start":0,"compacted_range_end":1,"summary_text":"two messages compacted","replacement_msg_seq":4,"ts":"2026-07-07T00:00:04Z"}"#;
        let after = r#"{"type":"user_msg","seq":6,"turn_id":"019f0000-0000-7000-0000-000000000003","message":{"role":"user","parts":[{"type":"text","text":"after compact"}],"turn_id":"019f0000-0000-7000-0000-000000000003"},"ts":"2026-07-07T00:00:05Z"}"#;
        write_events(dir.path(), &[user1, asst1, user2, summary, compact, after]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        // compact range 0-1 removes user1+asst1; user2 (outside range) + summary + after = 3
        assert_eq!(msgs.len(), 3, "compact summary + user2 + after compact");
        assert!(
            matches!(
                msgs[0].parts[0],
                crate::message::MessagePart::CompactSummary { .. }
            ),
            "first should be compact summary"
        );
        if let crate::message::MessagePart::CompactSummary { summary, .. } = &msgs[0].parts[0] {
            assert_eq!(summary, "two messages compacted");
        }
        assert_eq!(msgs[1].text_concat(), "old u2");
        assert_eq!(msgs[2].text_concat(), "after compact");
    }

    #[test]
    fn replay_messages_from_no_replacement_seq_ignores_compact() {
        let dir = TempDir::new().unwrap();
        let user1 = r#"{"type":"user_msg","seq":1,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"text","text":"hello"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        // context_compact with no replacement_msg_seq → should be ignored
        let compact = r#"{"type":"context_compact","seq":2,"session_id":"sess","before_tokens":200,"after_tokens":50,"compacted_range_start":0,"compacted_range_end":0,"summary_text":"ignored","replacement_msg_seq":null,"ts":"2026-07-07T00:00:01Z"}"#;
        write_events(dir.path(), &[user1, compact]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert_eq!(msgs.len(), 1, "compact without replacement seq is ignored");
        assert_eq!(msgs[0].text_concat(), "hello");
    }

    #[test]
    fn replay_messages_from_compact_after_no_change_ignored() {
        let dir = TempDir::new().unwrap();
        let user1 = r#"{"type":"user_msg","seq":1,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"text","text":"hello"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        let summary = r#"{"type":"system_msg","seq":2,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"system","parts":[{"type":"compact_summary","summary":"no change","seq_start":0,"seq_end":0,"count":1}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:01Z"}"#;
        // after_tokens >= before_tokens → compaction is no-op
        let compact = r#"{"type":"context_compact","seq":3,"session_id":"sess","before_tokens":50,"after_tokens":100,"compacted_range_start":0,"compacted_range_end":0,"summary_text":"no change","replacement_msg_seq":2,"ts":"2026-07-07T00:00:02Z"}"#;
        write_events(dir.path(), &[user1, summary, compact]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert_eq!(msgs.len(), 2, "compact with after>=before is ignored");
    }

    #[test]
    fn replay_messages_from_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn replay_messages_from_empty_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        write_events(dir.path(), &[]);
        let msgs = replay_messages_from(&dir.path().join("events.jsonl")).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn replay_all_messages_with_seq_includes_compacted() {
        let dir = TempDir::new().unwrap();
        let user1 = r#"{"type":"user_msg","seq":1,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"text","text":"old"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        let summary = r#"{"type":"system_msg","seq":4,"turn_id":"019f0000-0000-7000-0000-000000000002","message":{"role":"system","parts":[{"type":"compact_summary","summary":"s","seq_start":0,"seq_end":0,"count":1}],"turn_id":"019f0000-0000-7000-0000-000000000002"},"ts":"2026-07-07T00:00:01Z"}"#;
        let compact = r#"{"type":"context_compact","seq":5,"session_id":"sess","before_tokens":200,"after_tokens":50,"compacted_range_start":0,"compacted_range_end":0,"summary_text":"s","replacement_msg_seq":4,"ts":"2026-07-07T00:00:02Z"}"#;
        write_events(dir.path(), &[user1, summary, compact]);
        let all = replay_all_messages_with_seq(&dir.path().join("events.jsonl")).unwrap();
        // all includes both user1 and summary — compaction NOT applied
        assert_eq!(all.len(), 2, "all messages preserved (no compaction)");
        assert_eq!(all[0].1.text_concat(), "old");
    }
}
