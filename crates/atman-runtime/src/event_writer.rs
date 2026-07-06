use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::event::Event;
use crate::index::AnchorIndex;
use crate::redact::Redactor;

const BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

pub struct EventWriter {
    handle: Option<JoinHandle<()>>,
    tx: mpsc::UnboundedSender<Event>,
    stop_tx: Option<oneshot::Sender<()>>,
    events_path: PathBuf,
}

impl EventWriter {
    pub fn spawn(session_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::spawn_with(session_dir, None)
    }

    pub fn spawn_with(
        session_dir: impl AsRef<Path>,
        redactor: Option<Arc<Redactor>>,
    ) -> std::io::Result<Self> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let events_path = session_dir.join("events.jsonl");
        std::fs::create_dir_all(&session_dir)?;
        let index = match AnchorIndex::open_session(&session_dir) {
            Ok(idx) => Some(Arc::new(idx)),
            Err(e) => {
                eprintln!(
                    "[atman] anchor index unavailable at {} — dual-write disabled: {e}",
                    session_dir.display()
                );
                None
            }
        };
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let file_path = events_path.clone();
        let index_clone = index.clone();
        let redactor_clone = redactor.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = writer_loop(rx, stop_rx, &file_path, index_clone, redactor_clone).await
            {
                eprintln!("[atman] event writer failed: {e}");
            }
        });
        Ok(Self {
            handle: Some(handle),
            tx,
            stop_tx: Some(stop_tx),
            events_path,
        })
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
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

async fn writer_loop(
    mut rx: mpsc::UnboundedReceiver<Event>,
    mut stop_rx: oneshot::Receiver<()>,
    path: &Path,
    index: Option<Arc<AnchorIndex>>,
    redactor: Option<Arc<Redactor>>,
) -> std::io::Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let mut buf = tokio::io::BufWriter::new(file);
    let mut since_flush: usize = 0;
    let mut flush_deadline = tokio::time::Instant::now() + FLUSH_INTERVAL;

    loop {
        tokio::select! {
            biased;
            _ = &mut stop_rx => {
                while let Ok(event) = rx.try_recv() {
                    write_event(&mut buf, &event, index.as_deref(), redactor.as_deref()).await?;
                }
                break;
            }
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        write_event(&mut buf, &event, index.as_deref(), redactor.as_deref()).await?;
                        since_flush += 1;
                        if since_flush >= BATCH_SIZE {
                            buf.flush().await?;
                            since_flush = 0;
                            flush_deadline = tokio::time::Instant::now() + FLUSH_INTERVAL;
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(flush_deadline) => {
                buf.flush().await?;
                since_flush = 0;
                flush_deadline = tokio::time::Instant::now() + FLUSH_INTERVAL;
            }
        }
    }
    buf.flush().await?;
    Ok(())
}

async fn write_event(
    buf: &mut tokio::io::BufWriter<tokio::fs::File>,
    event: &Event,
    index: Option<&AnchorIndex>,
    redactor: Option<&Redactor>,
) -> std::io::Result<()> {
    let line = serialize_event(event, redactor);
    buf.write_all(line.as_bytes()).await?;
    buf.write_all(b"\n").await?;
    if let Some(idx) = index
        && let Err(e) = insert_event_row(idx, event, &line)
    {
        eprintln!(
            "[atman] index event insert failed (seq={}): {e}",
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

fn insert_event_row(
    index: &AnchorIndex,
    event: &Event,
    payload_json: &str,
) -> rusqlite::Result<()> {
    let seq = event.seq() as i64;
    let ts = extract_ts(event);
    let kind = event_kind(event);
    let (turn_id, flow_run_id) = extract_anchors(event);
    let text_content = extract_text_content(event);
    let conn = index.conn();
    conn.execute(
        "INSERT OR REPLACE INTO events (seq, ts, kind, turn_id, flow_run_id, payload) \
         VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![seq, ts, kind, turn_id, flow_run_id, payload_json,],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO events_fts (rowid, text_content) VALUES (?, ?)",
        rusqlite::params![seq, text_content.unwrap_or_default()],
    )?;
    Ok(())
}

fn extract_ts(event: &Event) -> String {
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
        | Event::FlowNodeEnd { ts, .. } => ts.to_rfc3339(),
    }
}

fn event_kind(event: &Event) -> &'static str {
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
    }
}

fn extract_anchors(event: &Event) -> (Option<String>, Option<String>) {
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
        | Event::FlowNodeEnd { run_id, .. } => (None, Some(run_id.0.to_string())),
        Event::LlmCall { .. }
        | Event::ContextCompact { .. }
        | Event::PendingPrompt { .. }
        | Event::PromptResolved { .. } => (None, None),
    }
}

fn extract_text_content(event: &Event) -> Option<String> {
    match event {
        Event::UserMsg { message, .. }
        | Event::AssistantMsg { message, .. }
        | Event::ToolResultMsg { message, .. }
        | Event::SystemMsg { message, .. } => Some(message.text_concat()),
        Event::WatchWarn { message, .. } => Some(message.clone()),
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
    async fn writer_dual_writes_to_session_anchor_index() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::spawn(dir.path()).unwrap();
        let tx = writer.sender();
        for i in 0..3 {
            tx.send(Event::FlowStart {
                seq: (i + 1) as u64,
                run_id: FlowRunId::now(),
                flow_name: format!("flow_{i}"),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        }
        drop(tx);
        writer.shutdown().await;

        let jsonl_lines = tokio::fs::read_to_string(dir.path().join("events.jsonl"))
            .await
            .unwrap()
            .lines()
            .count();
        assert_eq!(jsonl_lines, 3);

        let idx = crate::index::AnchorIndex::open_session(dir.path()).unwrap();
        let conn = idx.conn();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", rusqlite::params![], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 3, "sqlite events row count must match jsonl");

        let kinds: Vec<String> = conn
            .prepare("SELECT kind FROM events ORDER BY seq")
            .unwrap()
            .query_map(rusqlite::params![], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(kinds, vec!["flow_start"; 3]);
    }

    #[tokio::test]
    async fn writer_indexes_user_msg_text_for_fts() {
        use crate::event::TurnId;
        use crate::message::Message;

        let dir = TempDir::new().unwrap();
        let writer = EventWriter::spawn(dir.path()).unwrap();
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

        let idx = crate::index::AnchorIndex::open_session(dir.path()).unwrap();
        let conn = idx.conn();
        let matches: Vec<i64> = conn
            .prepare("SELECT rowid FROM events_fts WHERE events_fts MATCH ?")
            .unwrap()
            .query_map(rusqlite::params!["sqlite"], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(matches, vec![1], "fts should find seq=1 for `sqlite`");
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
