use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::Event;

const BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

pub struct EventWriter {
    handle: Option<JoinHandle<()>>,
    tx: mpsc::UnboundedSender<Event>,
    events_path: PathBuf,
}

impl EventWriter {
    pub fn spawn(session_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let events_path = session_dir.as_ref().join("events.jsonl");
        std::fs::create_dir_all(session_dir.as_ref())?;
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let file_path = events_path.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = writer_loop(rx, &file_path).await {
                eprintln!("[atman] event writer failed: {e}");
            }
        });
        Ok(Self {
            handle: Some(handle),
            tx,
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
        drop(self.tx.clone());
        if let Some(handle) = self.handle.take() {
            drop(std::mem::replace(&mut self.tx, mpsc::unbounded_channel().0));
            let _ = handle.await;
        }
    }
}

async fn writer_loop(mut rx: mpsc::UnboundedReceiver<Event>, path: &Path) -> std::io::Result<()> {
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
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        let line = serde_json::to_string(&event).unwrap_or_else(|e| {
                            format!("{{\"type\":\"encode_error\",\"error\":{:?}}}", e.to_string())
                        });
                        buf.write_all(line.as_bytes()).await?;
                        buf.write_all(b"\n").await?;
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
    async fn writer_serializes_flow_end_with_status() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::spawn(dir.path()).unwrap();
        writer
            .sender()
            .send(Event::FlowEnd {
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
