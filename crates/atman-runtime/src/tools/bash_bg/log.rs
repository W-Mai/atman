//! Publish checkpoints from the process's original log writer.

use std::{fs::File, time::Duration};

use tokio::{io::AsyncWriteExt, sync::mpsc, time::Instant};

use crate::{
    event::{Event, EventSink, FlowRunId},
    tool::ToolCtx,
};

const INTERVAL: Duration = Duration::from_secs(2);

pub(super) struct BashLogJournal {
    sink: EventSink,
    run: Option<FlowRunId>,
    handle: String,
    digest: blake3::Hasher,
    bytes: u64,
    published: u64,
    due: Instant,
}

impl BashLogJournal {
    pub(super) fn open(ctx: &ToolCtx, handle: &str) -> Option<Self> {
        ctx.instance_publication.as_ref()?;
        let sink = ctx.events.clone()?;
        let mut journal = Self {
            sink,
            run: ctx.flow_run_id.clone(),
            handle: handle.to_owned(),
            digest: blake3::Hasher::new(),
            bytes: 0,
            published: 0,
            due: Instant::now(),
        };
        journal.publish(false);
        Some(journal)
    }

    fn publish(&mut self, drained: bool) {
        self.sink.emit(Event::BashLogCheckpoint {
            flow_run_id: self.run.clone(),
            handle: self.handle.clone(),
            bytes: self.bytes,
            digest: *self.digest.finalize().as_bytes(),
            drained,
        });
        self.published = self.bytes;
        self.due = Instant::now() + INTERVAL;
    }

    fn deadline(&self) -> Option<Instant> {
        (self.bytes > self.published).then_some(self.due)
    }
}

pub(super) async fn write(
    file: File,
    mut frames: mpsc::Receiver<Vec<u8>>,
    mut journal: Option<BashLogJournal>,
) -> Result<(), String> {
    let mut file = tokio::fs::File::from_std(file);
    loop {
        let frame = match journal
            .as_mut()
            .filter(|journal| journal.deadline().is_some())
        {
            Some(journal) => tokio::select! {
                biased;
                _ = tokio::time::sleep_until(journal.due) => {
                    file.flush().await.map_err(|error| format!("flush log: {error}"))?;
                    journal.publish(false);
                    continue;
                }
                frame = frames.recv() => frame,
            },
            None => frames.recv().await,
        };
        let Some(frame) = frame else { break };
        file.write_all(&frame)
            .await
            .map_err(|error| format!("write log: {error}"))?;
        if let Some(journal) = &mut journal {
            journal.digest.update(&frame);
            journal.bytes += frame.len() as u64;
            if journal.published == 0 && journal.bytes > 0 {
                file.flush()
                    .await
                    .map_err(|error| format!("flush log: {error}"))?;
                journal.publish(false);
            }
        }
    }
    file.flush()
        .await
        .map_err(|error| format!("flush log: {error}"))?;
    if let Some(journal) = &mut journal {
        journal.publish(true);
    }
    Ok(())
}
