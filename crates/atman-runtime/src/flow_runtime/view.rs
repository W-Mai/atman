use std::{io, num::NonZeroUsize, sync::Arc};

use futures::{StreamExt, stream::BoxStream};
use tokio::sync::watch;

use super::{FlowWorker, InstanceStatus};
use crate::{
    event_log::journal::{CommitReceipt, JournalPage},
    flow_store::{inbox::AcceptedEnvelope, planning::PlanningSnapshot, view::FlowUpdate},
    projection::flow::FlowTranscript,
};

/// Durable instance state at one committed cursor. Live resources and client drafts are observed separately.
#[derive(Clone, Debug)]
pub struct FlowViewSnapshot {
    pub status: InstanceStatus,
    pub planning: PlanningSnapshot,
    pub pending: Vec<AcceptedEnvelope>,
}

/// Conversation and state from the same committed boundary, shared by all kinds of flow views.
pub struct FlowView {
    transcript: FlowTranscript,
    snapshot: Option<Arc<FlowViewSnapshot>>,
}

impl FlowView {
    pub fn transcript(&self) -> &FlowTranscript {
        &self.transcript
    }

    pub fn snapshot(&self) -> &Arc<FlowViewSnapshot> {
        // Readers expose the view only after both sides reach the captured cursor.
        self.snapshot.as_ref().expect("committed view snapshot")
    }
}

/// A cancel-safe, paged reader. Interrupted catch-up remains private until the captured boundary is complete; dropping a reader does not pause or close execution.
pub struct FlowViewReader {
    worker: FlowWorker,
    commits: watch::Receiver<CommitReceipt>,
    limit: NonZeroUsize,
    view: FlowView,
    target: Option<Arc<FlowViewSnapshot>>,
    capture_pending: bool,
}

impl FlowViewReader {
    /// A completed projection, or None while an interrupted read has only partially caught up.
    pub fn view(&self) -> Option<&FlowView> {
        self.view.snapshot.as_ref().and_then(|snapshot| {
            (self.view.transcript.cursor() == Some(&snapshot.status.cursor)).then_some(&self.view)
        })
    }

    /// Await a changed committed boundary. Cancellation preserves both the catch-up position and an observed notification that has not yet been read.
    pub async fn next(&mut self) -> io::Result<&FlowView> {
        loop {
            if self.target.is_none() {
                if !self.capture_pending {
                    self.commits.changed().await.map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "flow commit source closed")
                    })?;
                    self.capture_pending = true;
                }
                self.commits.borrow_and_update();
                self.target = Some(Arc::new(self.worker.view_snapshot().await?));
                self.capture_pending = false;
            }
            if self.view().is_some_and(|view| {
                view.snapshot().status.cursor == self.target.as_ref().unwrap().status.cursor
            }) {
                self.target = None;
                continue;
            }
            self.catch_up().await?;
            return Ok(&self.view);
        }
    }

    /// Capture current committed state without waiting for a notification, including after host quiescence. Repeated refreshes retain existing record identities.
    pub async fn refresh(&mut self) -> io::Result<&FlowView> {
        self.capture_pending = true;
        self.commits.borrow_and_update();
        self.target = Some(Arc::new(self.worker.view_snapshot().await?));
        self.capture_pending = false;
        self.catch_up().await?;
        Ok(&self.view)
    }

    async fn catch_up(&mut self) -> io::Result<()> {
        let target = &self
            .target
            .as_ref()
            .expect("captured view boundary")
            .status
            .cursor;
        while self.view.transcript.cursor() != Some(target) {
            let after = self.view.transcript.cursor().cloned();
            let remaining = target
                .sequence
                .checked_sub(after.as_ref().map_or(0, |c| c.sequence))
                .filter(|remaining| *remaining > 0)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "flow view cursor conflicts with captured state",
                    )
                })?;
            let limit = self
                .limit
                .get()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            let page = self
                .worker
                .read_updates(after, NonZeroUsize::new(limit).unwrap())
                .await?;
            if page.batches.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "flow view history ends before captured state",
                ));
            }
            for batch in page.batches {
                self.view.transcript.apply(batch)?;
            }
            // Long histories release the owner between pages and cooperate with other UI work.
            tokio::task::yield_now().await;
        }
        self.view.snapshot = self.target.take();
        Ok(())
    }
}

struct Feed {
    worker: FlowWorker,
    commits: watch::Receiver<CommitReceipt>,
    initial: Option<JournalPage<FlowUpdate>>,
    after: Option<CommitReceipt>,
    limit: NonZeroUsize,
    more: bool,
}

impl FlowWorker {
    pub async fn view_snapshot(&self) -> io::Result<FlowViewSnapshot> {
        self.request(|store| {
            Ok(FlowViewSnapshot {
                status: InstanceStatus::from_store(store),
                planning: store.planning().clone(),
                pending: store.pending_inputs().cloned().collect(),
            })
        })
        .await
    }

    /// Subscribe before capturing state. Paged catch-up stops at that snapshot's cursor even while new commits arrive; subsequent changes remain available for the next read.
    pub async fn subscribe_view(&self, limit: NonZeroUsize) -> io::Result<FlowViewReader> {
        let mut commits = self.subscribe_commits();
        commits.borrow_and_update();
        let target = self.view_snapshot().await?;
        Ok(FlowViewReader {
            worker: self.clone(),
            commits,
            limit,
            view: FlowView {
                transcript: FlowTranscript::new(self.address().clone()),
                snapshot: None,
            },
            target: Some(Arc::new(target)),
            capture_pending: false,
        })
    }

    /// Read an exclusive, instance-bound cursor through the storage owner. Pages preserve commit order and never start, resume, or consume flow work.
    pub async fn read_updates(
        &self,
        after: Option<CommitReceipt>,
        limit: NonZeroUsize,
    ) -> io::Result<JournalPage<FlowUpdate>> {
        self.request(move |store| store.read_updates(after.as_ref(), limit))
            .await
    }

    /// Establish commit observation and validate the initial page before returning the stream. Catch-up reads and later notifications use one cursor; dropping the reader leaves execution untouched.
    pub async fn subscribe_updates(
        &self,
        after: Option<CommitReceipt>,
        limit: NonZeroUsize,
    ) -> io::Result<BoxStream<'static, io::Result<JournalPage<FlowUpdate>>>> {
        let mut commits = self.subscribe_commits();
        commits.borrow_and_update();
        let initial = self.read_updates(after.clone(), limit).await?;
        let feed = Feed {
            worker: self.clone(),
            commits,
            initial: Some(initial),
            after,
            limit,
            more: false,
        };
        Ok(futures::stream::try_unfold(feed, |mut feed| async move {
            loop {
                let page = if let Some(initial) = feed.initial.take() {
                    initial
                } else {
                    if !feed.more && feed.commits.changed().await.is_err() {
                        return Ok(None);
                    }
                    feed.commits.borrow_and_update();
                    feed.worker
                        .read_updates(feed.after.clone(), feed.limit)
                        .await?
                };
                feed.after = page.next.clone();
                feed.more = page.has_more;
                if !page.batches.is_empty() {
                    return Ok(Some((page, feed)));
                }
            }
        })
        .boxed())
    }
}
