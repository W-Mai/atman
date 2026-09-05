use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::{mpsc, oneshot};

use crate::event::{Event, EventEnvelope};
use crate::index::{AnchorIndex, ProjectEventInsert};
use crate::redact::Redactor;

pub struct EventWriter {
    thread: Option<std::thread::JoinHandle<()>>,
    tx: mpsc::UnboundedSender<EventEnvelope>,
    flush_tx: mpsc::UnboundedSender<oneshot::Sender<EventWriterWatermark>>,
    stop_tx: Option<oneshot::Sender<()>>,
    events_path: PathBuf,
    watermark: Arc<std::sync::Mutex<EventWriterWatermark>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventWriterWatermark {
    pub seq: u64,
    pub offset: u64,
}

struct WriterPosition {
    offset: u64,
    watermark: Arc<std::sync::Mutex<EventWriterWatermark>>,
}

struct WriterChannels {
    events: mpsc::UnboundedReceiver<EventEnvelope>,
    flushes: mpsc::UnboundedReceiver<oneshot::Sender<EventWriterWatermark>>,
    stop: oneshot::Receiver<()>,
}

impl WriterPosition {
    fn new(offset: u64, watermark: Arc<std::sync::Mutex<EventWriterWatermark>>) -> Self {
        watermark.lock().unwrap().offset = offset;
        Self { offset, watermark }
    }

    fn commit(&mut self, seq: u64, offset: u64) {
        self.offset = offset;
        *self.watermark.lock().unwrap() = EventWriterWatermark { seq, offset };
    }
}

impl EventWriter {
    pub fn spawn(session_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::spawn_full(session_dir, None, None, None)
    }

    pub fn spawn_with(
        session_dir: impl AsRef<Path>,
        redactor: Option<Arc<Redactor>>,
    ) -> std::io::Result<Self> {
        Self::spawn_full(session_dir, redactor, None, None)
    }

    // Owns its own thread + rt so short-lived caller runtimes
    // (spawn_blocking + throwaway current_thread rt) can't kill the loop.
    pub fn spawn_full(
        session_dir: impl AsRef<Path>,
        redactor: Option<Arc<Redactor>>,
        project_index: Option<Arc<AnchorIndex>>,
        session_id: Option<String>,
    ) -> std::io::Result<Self> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let events_path = session_dir.join("events.jsonl");
        std::fs::create_dir_all(&session_dir)?;
        let (tx, rx) = mpsc::unbounded_channel::<EventEnvelope>();
        let (flush_tx, flush_rx) =
            mpsc::unbounded_channel::<oneshot::Sender<EventWriterWatermark>>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let file_path = events_path.clone();
        let watermark = Arc::new(std::sync::Mutex::new(EventWriterWatermark::default()));
        let writer_watermark = watermark.clone();
        let thread = std::thread::Builder::new()
            .name("atman-event-writer".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        crate::notify!(error, "event writer rt init failed: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = writer_loop(
                        WriterChannels {
                            events: rx,
                            flushes: flush_rx,
                            stop: stop_rx,
                        },
                        &file_path,
                        project_index,
                        session_id,
                        redactor,
                        writer_watermark,
                    )
                    .await
                    {
                        crate::notify!(error, "event writer failed: {e}");
                    }
                });
            })?;
        Ok(Self {
            thread: Some(thread),
            tx,
            flush_tx,
            stop_tx: Some(stop_tx),
            events_path,
            watermark,
        })
    }

    pub async fn flush(&self) -> Option<EventWriterWatermark> {
        self.request_flush()?.await.ok()
    }

    pub(crate) fn request_flush(&self) -> Option<oneshot::Receiver<EventWriterWatermark>> {
        let (tx, rx) = oneshot::channel::<EventWriterWatermark>();
        if self.flush_tx.send(tx).is_err() {
            return None;
        }
        Some(rx)
    }

    pub(crate) fn restore_durable_seq(&self, seq: u64) {
        self.watermark.lock().unwrap().seq = seq;
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<EventEnvelope> {
        self.tx.clone()
    }

    pub fn events_path(&self) -> &Path {
        &self.events_path
    }

    pub async fn shutdown(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = thread.join();
            })
            .await;
        }
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const MAX_BUFFERED: usize = 10_000;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(2);

struct DegradedBuffer {
    events: VecDeque<EventEnvelope>,
    error: Option<String>,
    dropped: u64,
    max_buffered: usize,
}

impl DegradedBuffer {
    fn new(max_buffered: usize) -> Self {
        Self {
            events: VecDeque::new(),
            error: None,
            dropped: 0,
            max_buffered,
        }
    }

    fn is_degraded(&self) -> bool {
        self.error.is_some()
    }

    fn degrade(&mut self, event: EventEnvelope, error: impl Into<String>) {
        self.error = Some(error.into());
        self.buffer(event);
    }

    fn buffer(&mut self, event: EventEnvelope) {
        if self.events.len() == self.max_buffered {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(event);
    }

    fn recover(&mut self) {
        self.error = None;
    }
}

async fn writer_loop(
    mut channels: WriterChannels,
    path: &Path,
    project_index: Option<Arc<AnchorIndex>>,
    session_id: Option<String>,
    redactor: Option<Arc<Redactor>>,
    watermark: Arc<std::sync::Mutex<EventWriterWatermark>>,
) -> std::io::Result<()> {
    // O_APPEND prevents seek-based overwrites required for idempotent retries.
    #[allow(clippy::suspicious_open_options)]
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .await?;
    let offset = file.seek(SeekFrom::End(0)).await?;
    let mut position = WriterPosition::new(offset, watermark.clone());
    let session_dir = path.parent().unwrap_or(path);
    let indexer = project_index.zip(session_id);
    let mut degraded = DegradedBuffer::new(MAX_BUFFERED);
    let mut recovery_tick = tokio::time::interval(RECOVERY_INTERVAL);

    loop {
        tokio::select! {
            biased;
            _ = &mut channels.stop => {
                while let Ok(event) = channels.events.try_recv() {
                    handle_event(
                        &mut file,
                        &mut position,
                        event,
                        &mut degraded,
                        indexer.as_ref(),
                        redactor.as_deref(),
                        session_dir,
                    ).await;
                }
                while let Ok(waiter) = channels.flushes.try_recv() {
                    let _ = waiter.send(*watermark.lock().unwrap());
                }
                break;
            }
            _ = recovery_tick.tick() => {
                retry_degraded_on_tick(
                    &mut file,
                    &mut position,
                    &mut degraded,
                    indexer.as_ref(),
                    redactor.as_deref(),
                    session_dir,
                ).await;
            }
            maybe_event = channels.events.recv() => {
                match maybe_event {
                    Some(event) => {
                        handle_event(
                            &mut file,
                            &mut position,
                            event,
                            &mut degraded,
                            indexer.as_ref(),
                            redactor.as_deref(),
                            session_dir,
                        ).await;
                    }
                    None => break,
                }
            }
            maybe_flush = channels.flushes.recv() => {
                match maybe_flush {
                    Some(waiter) => {
                        while let Ok(event) = channels.events.try_recv() {
                            handle_event(
                                &mut file,
                                &mut position,
                                event,
                                &mut degraded,
                                indexer.as_ref(),
                                redactor.as_deref(),
                                session_dir,
                            ).await;
                        }
                        retry_buffered(
                            &mut file,
                            &mut position,
                            &mut degraded,
                            indexer.as_ref(),
                            redactor.as_deref(),
                            session_dir,
                        ).await;
                        if let Err(e) = file.sync_data().await {
                            crate::notify!(error, "event writer flush failed: {e}");
                        }
                        let _ = waiter.send(*watermark.lock().unwrap());
                    }
                    None => break,
                }
            }
        }
    }
    retry_buffered(
        &mut file,
        &mut position,
        &mut degraded,
        indexer.as_ref(),
        redactor.as_deref(),
        session_dir,
    )
    .await;
    if !degraded.events.is_empty() {
        crate::notify!(
            error,
            "event writer stopped with {} buffered event(s) not persisted (dropped {}): {}",
            degraded.events.len(),
            degraded.dropped,
            degraded.error.as_deref().unwrap_or("write failed")
        );
    }
    if let Err(e) = file.sync_data().await {
        crate::notify!(error, "event writer final sync failed: {e}");
    }
    Ok(())
}

async fn handle_event(
    file: &mut tokio::fs::File,
    position: &mut WriterPosition,
    event: EventEnvelope,
    degraded: &mut DegradedBuffer,
    indexer: Option<&(Arc<AnchorIndex>, String)>,
    redactor: Option<&Redactor>,
    session_dir: &Path,
) {
    if degraded.is_degraded() {
        degraded.buffer(event);
        return;
    }
    if let Err(e) = write_event(file, position, &event, indexer, redactor, session_dir).await {
        crate::notify!(
            error,
            "event writer write failed (seq={}): {e}; buffering events in memory",
            event.seq
        );
        degraded.degrade(event, e.to_string());
    }
}

async fn retry_degraded_on_tick(
    file: &mut tokio::fs::File,
    position: &mut WriterPosition,
    degraded: &mut DegradedBuffer,
    indexer: Option<&(Arc<AnchorIndex>, String)>,
    redactor: Option<&Redactor>,
    session_dir: &Path,
) {
    if !degraded.is_degraded() || degraded.events.is_empty() {
        return;
    }

    retry_buffered(file, position, degraded, indexer, redactor, session_dir).await;
    if !degraded.is_degraded() {
        crate::notify!(info, location = Status, "事件写入已恢复");
    }
}

async fn retry_buffered(
    file: &mut tokio::fs::File,
    position: &mut WriterPosition,
    degraded: &mut DegradedBuffer,
    indexer: Option<&(Arc<AnchorIndex>, String)>,
    redactor: Option<&Redactor>,
    session_dir: &Path,
) {
    while let Some(event) = degraded.events.pop_front() {
        if let Err(e) = write_event(file, position, &event, indexer, redactor, session_dir).await {
            crate::notify!(
                error,
                "event writer retry failed (seq={}): {e}; {} event(s) remain buffered",
                event.seq,
                degraded.events.len() + 1
            );
            degraded.events.push_front(event);
            degraded.error = Some(e.to_string());
            return;
        }
    }
    degraded.recover();
}

async fn write_event(
    file: &mut tokio::fs::File,
    position: &mut WriterPosition,
    envelope: &EventEnvelope,
    indexer: Option<&(Arc<AnchorIndex>, String)>,
    redactor: Option<&Redactor>,
    session_dir: &Path,
) -> std::io::Result<()> {
    let line = serialize_event(envelope, redactor);
    let start = position.offset;
    file.seek(SeekFrom::Start(start)).await?;
    if let Err(e) = file.write_all(line.as_bytes()).await {
        position.offset = start;
        return Err(e);
    }
    if let Err(e) = file.write_all(b"\n").await {
        position.offset = start;
        return Err(e);
    }
    let end = file.stream_position().await?;
    if let Err(e) = file.sync_data().await {
        position.offset = start;
        return Err(e);
    }
    position.commit(envelope.seq, end);
    if let Err(error) =
        crate::session_meta::SessionStats::record_persisted_event(session_dir, start, end, envelope)
    {
        crate::notify!(
            warn,
            location = Log,
            stack = merge_count("session_stats.persist_failed", 60_000),
            "session stats update failed (seq={}): {error}",
            envelope.seq
        );
    }
    if let Some((idx, sid)) = indexer
        && let Err(e) = insert_project_row(idx, sid, envelope, &line)
    {
        crate::notify!(
            warn,
            location = Log,
            stack = merge_count("project_index.insert_failed", 60_000),
            "project index insert failed (seq={}): {e}",
            envelope.seq
        );
    }
    Ok(())
}

fn serialize_event(envelope: &EventEnvelope, redactor: Option<&Redactor>) -> String {
    let Some(r) = redactor.filter(|_| {
        !matches!(
            envelope.event,
            Event::ContextCreated { .. } | Event::ContextHeadSelected { .. }
        )
    }) else {
        return serde_json::to_string(envelope).unwrap_or_else(|e| {
            format!(
                "{{\"type\":\"encode_error\",\"error\":{:?}}}",
                e.to_string()
            )
        });
    };
    let mut value = match serde_json::to_value(envelope) {
        Ok(v) => v,
        Err(e) => {
            return format!(
                "{{\"type\":\"encode_error\",\"error\":{:?}}}",
                e.to_string()
            );
        }
    };
    let metadata = ["seq", "ts", "context_id"].map(|key| {
        let field = value.as_object_mut().and_then(|map| map.remove(key));
        (key, field)
    });
    r.redact_json(&mut value);
    if let serde_json::Value::Object(map) = &mut value {
        for (key, field) in metadata {
            if let Some(field) = field {
                map.insert(key.into(), field);
            }
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|e| {
        format!(
            "{{\"type\":\"encode_error\",\"error\":{:?}}}",
            e.to_string()
        )
    })
}

fn insert_project_row(
    index: &AnchorIndex,
    session_id: &str,
    envelope: &EventEnvelope,
    payload_json: &str,
) -> rusqlite::Result<()> {
    let event = &envelope.event;
    let ts = extract_ts(envelope);
    let kind = event_kind(event);
    let (turn_id, flow_run_id) = extract_anchors(event);
    let text = extract_text_content(event).unwrap_or_default();
    index.insert_project_event_raw(ProjectEventInsert {
        session_id,
        seq: envelope.seq as i64,
        ts: &ts,
        kind,
        turn_id: turn_id.as_deref(),
        flow_run_id: flow_run_id.as_deref(),
        text_content: &text,
        payload_json,
    })?;
    Ok(())
}

pub(crate) fn extract_ts(envelope: &EventEnvelope) -> String {
    envelope.ts.to_rfc3339()
}

pub(crate) fn event_kind(event: &Event) -> &'static str {
    match event {
        Event::ContextCreated { .. } => "context_created",
        Event::ContextHeadSelected { .. } => "context_head_selected",
        Event::FlowStart { .. } => "flow_start",
        Event::FlowEnd { .. } => "flow_end",
        Event::RunCancelRequested { .. } => "run_cancel_requested",
        Event::GenerationReconciled { .. } => "generation_reconciled",
        Event::WorkspaceLifecycle { .. } => "workspace_lifecycle",
        Event::TaskLifecycle { .. } => "task_lifecycle",
        Event::TaskReaped { .. } => "task_reaped",
        Event::LlmCall { .. } => "llm_call",
        Event::TurnStart { .. } => "turn_start",
        Event::TurnEnd { .. } => "turn_end",
        Event::UserMsg { .. } => "user_msg",
        Event::AssistantMsg { .. } => "assistant_msg",
        Event::ToolResultMsg { .. } => "tool_result_msg",
        Event::ToolResultMetrics { .. } => "tool_result_metrics",
        Event::DiffPreview { .. } => "diff_preview",
        Event::FileEditApplied { .. } => "file_edit_applied",
        Event::CompactionStarted { .. } => "compaction_started",
        Event::CompactionSummary { .. } => "compaction_summary",
        Event::CompactionFailed { .. } => "compaction_failed",
        Event::SystemMsg { .. } => "system_msg",
        Event::UserInject { .. } => "user_inject",
        Event::ContentFilterHit { .. } => "content_filter_hit",
        Event::ContextCompact { .. } => "context_compact",
        Event::Checkpoint { .. } => "checkpoint",
        Event::ContextTruncated { .. } => "context_truncated",
        Event::WatchWarn { .. } => "watch_warn",
        Event::PendingPrompt { .. } => "pending_prompt",
        Event::PromptResolved { .. } => "prompt_resolved",
        Event::FormRequested { .. } => "form_requested",
        Event::FormResolved { .. } => "form_resolved",
        Event::CompactReviewRequested { .. } => "compact_review_requested",
        Event::CompactReviewResolved { .. } => "compact_review_resolved",
        Event::LlmPartialCall { .. } => "llm_partial_call",
        Event::FlowGraph { .. } => "flow_graph",
        Event::FlowNodeStart { .. } => "flow_node_start",
        Event::FlowNodeEnd { .. } => "flow_node_end",
        Event::ToolNode { .. } => "tool_node",
        Event::AttachmentDegraded { .. } => "attachment_degraded",
        Event::ToolPendingApproval { .. } => "tool_pending_approval",
        Event::ToolApproved { .. } => "tool_approved",
        Event::ToolDenied { .. } => "tool_denied",
        Event::PermissionRequestCreated { .. } => "permission_request_created",
        Event::PermissionRequestTargeted { .. } => "permission_request_targeted",
        Event::PermissionRequestDeferred { .. } => "permission_request_deferred",
        Event::PermissionRequestApproved { .. } => "permission_request_approved",
        Event::PermissionRequestDenied { .. } => "permission_request_denied",
        Event::PermissionRequestCancelled { .. } => "permission_request_cancelled",
        Event::PermissionGroupCreated { .. } => "permission_group_created",
        Event::PermissionGroupUpdated { .. } => "permission_group_updated",
        Event::PermissionGroupResolved { .. } => "permission_group_resolved",
        Event::PermissionGrantCreated { .. } => "permission_grant_created",
        Event::PermissionGrantExpired { .. } => "permission_grant_expired",
        Event::UnrestrictedExecution { .. } => "unrestricted_execution",
        Event::TerminalFinalState { .. } => "terminal_final_state",
        Event::MermaidDiagram { .. } => "mermaid_diagram",
    }
}

pub(crate) fn extract_anchors(event: &Event) -> (Option<String>, Option<String>) {
    match event {
        Event::FlowStart {
            run_id, turn_id, ..
        } => (
            turn_id.as_ref().map(ToString::to_string),
            Some(run_id.0.to_string()),
        ),
        Event::FlowEnd { run_id, .. }
        | Event::RunCancelRequested { run_id }
        | Event::WorkspaceLifecycle { run_id, .. } => (None, Some(run_id.0.to_string())),
        Event::TaskLifecycle { run_id, .. } => {
            (None, run_id.as_ref().map(|run_id| run_id.0.to_string()))
        }
        Event::TurnStart { turn_id, .. }
        | Event::TurnEnd { turn_id, .. }
        | Event::ContextHeadSelected { turn_id } => (Some(turn_id.0.to_string()), None),
        Event::UserInject {
            turn_id, injection, ..
        } => (
            Some(turn_id.0.to_string()),
            injection
                .flow_run_id
                .as_ref()
                .map(|run_id| run_id.0.to_string()),
        ),
        Event::UserMsg {
            turn_id,
            flow_run_id,
            ..
        }
        | Event::AssistantMsg {
            turn_id,
            flow_run_id,
            ..
        }
        | Event::ToolResultMsg {
            turn_id,
            flow_run_id,
            ..
        }
        | Event::ToolResultMetrics {
            turn_id,
            flow_run_id,
            ..
        }
        | Event::SystemMsg {
            turn_id,
            flow_run_id,
            ..
        } => (
            Some(turn_id.0.to_string()),
            flow_run_id.as_ref().map(|r| r.0.to_string()),
        ),
        Event::DiffPreview {
            turn_id,
            flow_run_id,
            ..
        } => (
            turn_id.as_ref().map(|t| t.0.to_string()),
            flow_run_id.as_ref().map(|r| r.0.to_string()),
        ),
        Event::FileEditApplied {
            turn_id,
            flow_run_id,
            ..
        } => (
            turn_id.as_ref().map(|t| t.0.to_string()),
            flow_run_id.as_ref().map(|r| r.0.to_string()),
        ),
        Event::ContentFilterHit {
            turn_id,
            flow_run_id,
            ..
        }
        | Event::ContextTruncated {
            turn_id,
            flow_run_id,
            ..
        }
        | Event::WatchWarn {
            turn_id,
            flow_run_id,
            ..
        } => (
            turn_id.as_ref().map(|t| t.0.to_string()),
            flow_run_id.as_ref().map(|r| r.0.to_string()),
        ),
        Event::LlmPartialCall {
            turn_id,
            flow_run_id,
            ..
        } => (
            turn_id.as_ref().map(|t| t.0.to_string()),
            flow_run_id.as_ref().map(|r| r.0.to_string()),
        ),
        Event::FlowGraph { run_id, .. }
        | Event::FlowNodeStart { run_id, .. }
        | Event::FlowNodeEnd { run_id, .. }
        | Event::ToolNode { run_id, .. }
        | Event::ToolPendingApproval { run_id, .. }
        | Event::ToolApproved { run_id, .. }
        | Event::ToolDenied { run_id, .. } => (None, Some(run_id.0.to_string())),
        Event::PermissionRequestCreated { payload }
        | Event::PermissionRequestTargeted { payload }
        | Event::PermissionRequestDeferred { payload }
        | Event::PermissionRequestApproved { payload }
        | Event::PermissionRequestDenied { payload }
        | Event::PermissionRequestCancelled { payload }
        | Event::UnrestrictedExecution { payload } => {
            (None, Some(payload.requesting_run_id.0.to_string()))
        }
        Event::PermissionGroupCreated { payload }
        | Event::PermissionGroupUpdated { payload }
        | Event::PermissionGroupResolved { payload } => {
            let anchor = match &payload.owner {
                crate::permission_audit::PermissionGroupAuditOwner::Flow { run_id } => {
                    run_id.to_string()
                }
                crate::permission_audit::PermissionGroupAuditOwner::User { session_id } => {
                    format!("user:{session_id}")
                }
                crate::permission_audit::PermissionGroupAuditOwner::System => "system".into(),
            };
            (None, Some(anchor))
        }
        Event::PermissionGrantCreated { payload } | Event::PermissionGrantExpired { payload } => {
            (None, Some(payload.requesting_run_id.0.to_string()))
        }
        Event::AttachmentDegraded {
            turn_id,
            flow_run_id,
            ..
        } => (
            turn_id.as_ref().map(|t| t.0.to_string()),
            flow_run_id.as_ref().map(|r| r.0.to_string()),
        ),
        Event::CompactionStarted { flow_run_id, .. }
        | Event::CompactionSummary { flow_run_id, .. }
        | Event::CompactionFailed { flow_run_id, .. }
        | Event::ContextCompact { flow_run_id, .. }
        | Event::Checkpoint { flow_run_id, .. } => (
            None,
            flow_run_id.as_ref().map(|run_id| run_id.0.to_string()),
        ),
        Event::FormRequested { form } => (None, Some(form.run_id.0.to_string())),
        Event::FormResolved { run_id, .. } => (None, Some(run_id.0.to_string())),
        Event::LlmCall { .. }
        | Event::GenerationReconciled { .. }
        | Event::PendingPrompt { .. }
        | Event::PromptResolved { .. }
        | Event::CompactReviewRequested { .. }
        | Event::CompactReviewResolved { .. }
        | Event::TerminalFinalState { .. }
        | Event::MermaidDiagram { .. }
        | Event::TaskReaped { .. }
        | Event::ContextCreated { .. } => (None, None),
    }
}

pub(crate) fn extract_text_content(event: &Event) -> Option<String> {
    if let Some((message, _)) = event.context_message() {
        return Some(message.text_concat());
    }
    match event {
        Event::WatchWarn { message, .. } => Some(message.clone()),
        Event::CompactionSummary { summary, .. } => Some(summary.clone()),
        Event::CompactionFailed { reason, .. } => Some(reason.clone()),
        Event::GenerationReconciled { reason, .. } => Some(reason.clone()),
        Event::AttachmentDegraded { patch, .. } => {
            Some(format!("{} {}", patch.file_basename, patch.reason))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, FlowRunId, FlowStatus};
    use tempfile::TempDir;

    fn flow_start(seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            seq,
            Event::FlowStart {
                turn_id: None,
                run_id: FlowRunId::now(),
                flow_name: format!("flow_{seq}"),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
            },
        )
    }

    #[test]
    fn every_owned_message_role_keeps_its_flow_anchor() {
        let turn_id = crate::event::TurnId::now();
        let flow_run_id = FlowRunId::now();
        let user = crate::message::Message::user_text(turn_id.clone(), "delegated");
        let system = crate::message::Message::system_text(turn_id.clone(), "handoff");

        for event in [
            Event::UserMsg {
                turn_id: turn_id.clone(),
                flow_run_id: Some(flow_run_id.clone()),
                message: user,
            },
            Event::SystemMsg {
                turn_id: turn_id.clone(),
                flow_run_id: Some(flow_run_id.clone()),
                message: system,
            },
        ] {
            assert_eq!(
                extract_anchors(&event),
                (Some(turn_id.to_string()), Some(flow_run_id.to_string()))
            );
        }
    }

    #[test]
    fn every_owned_compaction_event_keeps_its_flow_anchor() {
        let flow_run_id = FlowRunId::now();
        for event in [
            Event::CompactionStarted {
                operation_id: crate::event::CompactionOperationId::now(),
                flow_run_id: Some(flow_run_id.clone()),
                range_start: 0,
                range_end: 1,
                compacted_count: 2,
                before_tokens: 100,
            },
            Event::CompactionSummary {
                operation_id: Some(crate::event::CompactionOperationId::now()),
                session_id: "session".into(),
                flow_run_id: Some(flow_run_id.clone()),
                range_start: 0,
                range_end: 1,
                compacted_count: 2,
                before_tokens: 100,
                after_tokens: 10,
                summary: "summary".into(),
            },
            Event::CompactionFailed {
                operation_id: crate::event::CompactionOperationId::now(),
                flow_run_id: Some(flow_run_id.clone()),
                range_start: 0,
                range_end: 1,
                compacted_count: 2,
                before_tokens: 100,
                reason: "cancelled".into(),
            },
            Event::ContextCompact {
                session_id: "session".into(),
                flow_run_id: Some(flow_run_id.clone()),
                before_tokens: 100,
                after_tokens: 10,
                compacted_range_start: 0,
                compacted_range_end: 1,
                summary_text: Some("summary".into()),
                replacement_msg_seq: None,
            },
            Event::Checkpoint {
                session_id: "session".into(),
                flow_run_id: Some(flow_run_id.clone()),
                messages: Vec::new(),
                window_tokens: 10,
            },
        ] {
            assert_eq!(
                extract_anchors(&event),
                (None, Some(flow_run_id.to_string()))
            );
        }
    }

    #[test]
    fn degraded_buffer_keeps_later_events_and_drops_oldest_at_capacity() {
        let mut degraded = DegradedBuffer::new(2);

        degraded.degrade(flow_start(1), "disk full");
        degraded.buffer(flow_start(2));

        assert!(degraded.is_degraded());
        assert_eq!(
            degraded
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(degraded.dropped, 0);

        degraded.buffer(flow_start(3));

        assert_eq!(
            degraded
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(degraded.dropped, 1);
        assert_eq!(degraded.error.as_deref(), Some("disk full"));
    }

    #[tokio::test]
    async fn recovery_tick_retries_buffered_events() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.jsonl");
        tokio::fs::write(&path, b"").await.unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let mut degraded = DegradedBuffer::new(MAX_BUFFERED);
        let watermark = Arc::new(std::sync::Mutex::new(EventWriterWatermark::default()));
        let mut position = WriterPosition::new(0, watermark.clone());

        handle_event(
            &mut file,
            &mut position,
            flow_start(1),
            &mut degraded,
            None,
            None,
            dir.path(),
        )
        .await;

        assert!(degraded.is_degraded());
        assert_eq!(degraded.events.len(), 1);

        drop(file);
        #[allow(clippy::suspicious_open_options)]
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await
            .unwrap();
        let mut position = WriterPosition::new(0, watermark);
        retry_degraded_on_tick(
            &mut file,
            &mut position,
            &mut degraded,
            None,
            None,
            dir.path(),
        )
        .await;

        assert!(!degraded.is_degraded());
        assert!(degraded.events.is_empty());
        let contents = tokio::fs::read_to_string(path).await.unwrap();
        let event: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(event["seq"], 1);
        assert_eq!(event["type"], "flow_start");
    }

    #[tokio::test]
    async fn write_event_overwrites_partial_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .await
            .unwrap();
        let event = flow_start(1);
        let line = serialize_event(&event, None);
        file.write_all(&line.as_bytes()[..line.len() / 2])
            .await
            .unwrap();
        let watermark = Arc::new(std::sync::Mutex::new(EventWriterWatermark::default()));
        let mut position = WriterPosition::new(0, watermark);

        write_event(&mut file, &mut position, &event, None, None, dir.path())
            .await
            .unwrap();

        let contents = tokio::fs::read_to_string(path).await.unwrap();
        assert_eq!(contents.lines().count(), 1);
        let _: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
    }

    #[tokio::test]
    async fn writer_appends_events_as_jsonl() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::spawn(dir.path()).unwrap();
        let tx = writer.sender();
        for i in 0..5 {
            tx.send(EventEnvelope::new(
                i as u64 + 1,
                Event::FlowStart {
                    turn_id: None,
                    run_id: FlowRunId::now(),
                    flow_name: format!("flow_{i}"),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            ))
            .unwrap();
        }
        let watermark = writer.flush().await.expect("writer is running");
        let persisted_len = tokio::fs::metadata(dir.path().join("events.jsonl"))
            .await
            .unwrap()
            .len();
        assert_eq!(watermark.seq, 5);
        assert_eq!(watermark.offset, persisted_len);
        drop(tx);
        writer.shutdown().await;
        let path = dir.path().join("events.jsonl");
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 5);
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["type"], "flow_start");
            assert!(v["run_id"].is_string());
            assert!(v["flow_name"].is_string());
        }
        let stats = crate::session_meta::SessionStats::load_or_rebuild(dir.path()).unwrap();
        assert_eq!(stats.event_count, 5);
        assert_eq!(stats.message_count, 0);
        assert_eq!(stats.event_bytes, std::fs::metadata(path).unwrap().len());
    }

    #[tokio::test]
    async fn writer_writes_to_project_index_with_session_id() {
        let session_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(project_dir.path()).unwrap());
        let writer = EventWriter::spawn_full(
            session_dir.path(),
            None,
            Some(idx.clone()),
            Some("sess-x".into()),
        )
        .unwrap();
        let tx = writer.sender();
        for i in 0..3 {
            tx.send(EventEnvelope::new(
                (i + 1) as u64,
                Event::FlowStart {
                    turn_id: None,
                    run_id: FlowRunId::now(),
                    flow_name: format!("flow_{i}"),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            ))
            .unwrap();
        }
        drop(tx);
        writer.shutdown().await;

        let jsonl_lines = tokio::fs::read_to_string(session_dir.path().join("events.jsonl"))
            .await
            .unwrap()
            .lines()
            .count();
        assert_eq!(jsonl_lines, 3);

        let conn = idx.conn();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?",
                rusqlite::params!["sess-x"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn writer_indexes_user_msg_text_for_project_fts() {
        use crate::event::TurnId;
        use crate::message::Message;

        let session_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();
        let idx = Arc::new(AnchorIndex::open_project(project_dir.path()).unwrap());
        let writer = EventWriter::spawn_full(
            session_dir.path(),
            None,
            Some(idx.clone()),
            Some("sess-x".into()),
        )
        .unwrap();
        let tid = TurnId::now();
        writer
            .sender()
            .send(EventEnvelope::new(
                1,
                Event::UserMsg {
                    turn_id: tid.clone(),
                    flow_run_id: None,
                    message: Message::user_text(tid, "sqlite fts full text search"),
                },
            ))
            .unwrap();
        writer.shutdown().await;

        let hits = idx
            .fts_search_project_events("sqlite", Some("sess-x"), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess-x");
        assert_eq!(hits[0].seq, 1);
        let stats = crate::session_meta::SessionStats::load_or_rebuild(session_dir.path()).unwrap();
        assert_eq!(stats.event_count, 1);
        assert_eq!(stats.user_message_count, 1);
        assert_eq!(stats.message_count, 1);
    }

    #[tokio::test]
    async fn writer_serializes_flow_end_with_status() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::spawn(dir.path()).unwrap();
        writer
            .sender()
            .send(EventEnvelope::new(
                0,
                Event::FlowEnd {
                    run_id: FlowRunId::now(),
                    flow_name: "t".into(),
                    status: FlowStatus::Errored {
                        message: "boom".into(),
                    },
                    output: None,
                },
            ))
            .unwrap();
        writer.shutdown().await;
        let contents = tokio::fs::read_to_string(dir.path().join("events.jsonl"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(v["type"], "flow_end");
        assert_eq!(v["status"]["kind"], "errored");
    }
}
