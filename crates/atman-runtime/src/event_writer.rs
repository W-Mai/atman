use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use crate::event::Event;
use crate::index::{AnchorIndex, ProjectEventInsert};
use crate::redact::Redactor;

pub struct EventWriter {
    thread: Option<std::thread::JoinHandle<()>>,
    tx: mpsc::UnboundedSender<Event>,
    flush_tx: mpsc::UnboundedSender<oneshot::Sender<()>>,
    stop_tx: Option<oneshot::Sender<()>>,
    events_path: PathBuf,
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
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let (flush_tx, flush_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let file_path = events_path.clone();
        let thread = std::thread::Builder::new()
            .name("atman-event-writer".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("[atman] event writer rt init failed: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = writer_loop(
                        rx,
                        flush_rx,
                        stop_rx,
                        &file_path,
                        project_index,
                        session_id,
                        redactor,
                    )
                    .await
                    {
                        eprintln!("[atman] event writer failed: {e}");
                    }
                });
            })?;
        Ok(Self {
            thread: Some(thread),
            tx,
            flush_tx,
            stop_tx: Some(stop_tx),
            events_path,
        })
    }

    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel::<()>();
        if self.flush_tx.send(tx).is_err() {
            return;
        }
        let _ = rx.await;
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<Event> {
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

async fn writer_loop(
    mut rx: mpsc::UnboundedReceiver<Event>,
    mut flush_rx: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    mut stop_rx: oneshot::Receiver<()>,
    path: &Path,
    project_index: Option<Arc<AnchorIndex>>,
    session_id: Option<String>,
    redactor: Option<Arc<Redactor>>,
) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let indexer = project_index.zip(session_id);

    loop {
        tokio::select! {
            biased;
            _ = &mut stop_rx => {
                while let Ok(event) = rx.try_recv() {
                    write_event(&mut file, &event, indexer.as_ref(), redactor.as_deref()).await?;
                }
                while let Ok(waiter) = flush_rx.try_recv() {
                    let _ = waiter.send(());
                }
                break;
            }
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        write_event(&mut file, &event, indexer.as_ref(), redactor.as_deref()).await?;
                    }
                    None => break,
                }
            }
            maybe_flush = flush_rx.recv() => {
                match maybe_flush {
                    Some(waiter) => {
                        while let Ok(event) = rx.try_recv() {
                            write_event(&mut file, &event, indexer.as_ref(), redactor.as_deref()).await?;
                        }
                        file.sync_data().await?;
                        let _ = waiter.send(());
                    }
                    None => break,
                }
            }
        }
    }
    file.sync_data().await?;
    Ok(())
}

async fn write_event(
    file: &mut tokio::fs::File,
    event: &Event,
    indexer: Option<&(Arc<AnchorIndex>, String)>,
    redactor: Option<&Redactor>,
) -> std::io::Result<()> {
    let line = serialize_event(event, redactor);
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    file.sync_data().await?;
    if let Some((idx, sid)) = indexer
        && let Err(e) = insert_project_row(idx, sid, event, &line)
    {
        eprintln!(
            "[atman] project index insert failed (seq={}): {e}",
            event.seq()
        );
    }
    Ok(())
}

fn serialize_event(event: &Event, redactor: Option<&Redactor>) -> String {
    let Some(r) = redactor else {
        return serde_json::to_string(event).unwrap_or_else(|e| {
            format!(
                "{{\"type\":\"encode_error\",\"error\":{:?}}}",
                e.to_string()
            )
        });
    };
    let mut value = match serde_json::to_value(event) {
        Ok(v) => v,
        Err(e) => {
            return format!(
                "{{\"type\":\"encode_error\",\"error\":{:?}}}",
                e.to_string()
            );
        }
    };
    r.redact_json(&mut value);
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
    event: &Event,
    payload_json: &str,
) -> rusqlite::Result<()> {
    let ts = extract_ts(event);
    let kind = event_kind(event);
    let (turn_id, flow_run_id) = extract_anchors(event);
    let text = extract_text_content(event).unwrap_or_default();
    index.insert_project_event_raw(ProjectEventInsert {
        session_id,
        seq: event.seq() as i64,
        ts: &ts,
        kind,
        turn_id: turn_id.as_deref(),
        flow_run_id: flow_run_id.as_deref(),
        text_content: &text,
        payload_json,
    })?;
    Ok(())
}

pub(crate) fn extract_ts(event: &Event) -> String {
    match event {
        Event::FlowStart { ts, .. }
        | Event::FlowEnd { ts, .. }
        | Event::LlmCall { ts, .. }
        | Event::TurnStart { ts, .. }
        | Event::TurnEnd { ts, .. }
        | Event::UserMsg { ts, .. }
        | Event::AssistantMsg { ts, .. }
        | Event::ToolResultMsg { ts, .. }
        | Event::SystemMsg { ts, .. }
        | Event::UserInject { ts, .. }
        | Event::ContentFilterHit { ts, .. }
        | Event::ContextCompact { ts, .. }
        | Event::ContextTruncated { ts, .. }
        | Event::WatchWarn { ts, .. }
        | Event::PendingPrompt { ts, .. }
        | Event::PromptResolved { ts, .. }
        | Event::LlmPartialCall { ts, .. }
        | Event::FlowGraph { ts, .. }
        | Event::FlowNodeStart { ts, .. }
        | Event::FlowNodeEnd { ts, .. }
        | Event::ToolNode { ts, .. }
        | Event::AttachmentDegraded { ts, .. }
        | Event::ToolPendingApproval { ts, .. }
        | Event::ToolApproved { ts, .. }
        | Event::ToolDenied { ts, .. } => ts.to_rfc3339(),
    }
}

pub(crate) fn event_kind(event: &Event) -> &'static str {
    match event {
        Event::FlowStart { .. } => "flow_start",
        Event::FlowEnd { .. } => "flow_end",
        Event::LlmCall { .. } => "llm_call",
        Event::TurnStart { .. } => "turn_start",
        Event::TurnEnd { .. } => "turn_end",
        Event::UserMsg { .. } => "user_msg",
        Event::AssistantMsg { .. } => "assistant_msg",
        Event::ToolResultMsg { .. } => "tool_result_msg",
        Event::SystemMsg { .. } => "system_msg",
        Event::UserInject { .. } => "user_inject",
        Event::ContentFilterHit { .. } => "content_filter_hit",
        Event::ContextCompact { .. } => "context_compact",
        Event::ContextTruncated { .. } => "context_truncated",
        Event::WatchWarn { .. } => "watch_warn",
        Event::PendingPrompt { .. } => "pending_prompt",
        Event::PromptResolved { .. } => "prompt_resolved",
        Event::LlmPartialCall { .. } => "llm_partial_call",
        Event::FlowGraph { .. } => "flow_graph",
        Event::FlowNodeStart { .. } => "flow_node_start",
        Event::FlowNodeEnd { .. } => "flow_node_end",
        Event::ToolNode { .. } => "tool_node",
        Event::AttachmentDegraded { .. } => "attachment_degraded",
        Event::ToolPendingApproval { .. } => "tool_pending_approval",
        Event::ToolApproved { .. } => "tool_approved",
        Event::ToolDenied { .. } => "tool_denied",
    }
}

pub(crate) fn extract_anchors(event: &Event) -> (Option<String>, Option<String>) {
    match event {
        Event::FlowStart { run_id, .. } | Event::FlowEnd { run_id, .. } => {
            (None, Some(run_id.0.to_string()))
        }
        Event::TurnStart { turn_id, .. } | Event::TurnEnd { turn_id, .. } => {
            (Some(turn_id.0.to_string()), None)
        }
        Event::UserMsg { turn_id, .. }
        | Event::SystemMsg { turn_id, .. }
        | Event::UserInject { turn_id, .. } => (Some(turn_id.0.to_string()), None),
        Event::AssistantMsg {
            turn_id,
            flow_run_id,
            ..
        }
        | Event::ToolResultMsg {
            turn_id,
            flow_run_id,
            ..
        } => (
            Some(turn_id.0.to_string()),
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
        Event::AttachmentDegraded {
            turn_id,
            flow_run_id,
            ..
        } => (
            turn_id.as_ref().map(|t| t.0.to_string()),
            flow_run_id.as_ref().map(|r| r.0.to_string()),
        ),
        Event::LlmCall { .. }
        | Event::ContextCompact { .. }
        | Event::PendingPrompt { .. }
        | Event::PromptResolved { .. } => (None, None),
    }
}

pub(crate) fn extract_text_content(event: &Event) -> Option<String> {
    match event {
        Event::UserMsg { message, .. }
        | Event::AssistantMsg { message, .. }
        | Event::ToolResultMsg { message, .. }
        | Event::SystemMsg { message, .. } => Some(message.text_concat()),
        Event::WatchWarn { message, .. } => Some(message.clone()),
        Event::AttachmentDegraded {
            file_basename,
            reason,
            ..
        } => Some(format!("{file_basename} {reason}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, FlowRunId, FlowStatus};
    use tempfile::TempDir;

    #[tokio::test]
    async fn writer_appends_events_as_jsonl() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::spawn(dir.path()).unwrap();
        let tx = writer.sender();
        for i in 0..5 {
            tx.send(Event::FlowStart {
                seq: 0,
                run_id: FlowRunId::now(),
                flow_name: format!("flow_{i}"),
                parent_run_id: None,
                parent_node_id: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        }
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
            tx.send(Event::FlowStart {
                seq: (i + 1) as u64,
                run_id: FlowRunId::now(),
                flow_name: format!("flow_{i}"),
                parent_run_id: None,
                parent_node_id: None,
                ts: chrono::Utc::now(),
            })
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
            .send(Event::UserMsg {
                seq: 1,
                turn_id: tid.clone(),
                message: Message::user_text(tid, "sqlite fts full text search"),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        writer.shutdown().await;

        let hits = idx
            .fts_search_project_events("sqlite", Some("sess-x"), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess-x");
        assert_eq!(hits[0].seq, 1);
    }

    #[tokio::test]
    async fn writer_serializes_flow_end_with_status() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::spawn(dir.path()).unwrap();
        writer
            .sender()
            .send(Event::FlowEnd {
                seq: 0,
                run_id: FlowRunId::now(),
                flow_name: "t".into(),
                status: FlowStatus::Errored {
                    message: "boom".into(),
                },
                ts: chrono::Utc::now(),
            })
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
