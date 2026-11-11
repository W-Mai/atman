use std::{io, num::NonZeroUsize, sync::Arc};

use futures::{StreamExt, stream::BoxStream};
use tokio::sync::watch;

mod projection;

use super::{FlowWorker, InstanceStatus};
use crate::{
    flow_store::position::{FlowPage, FlowPosition},
    flow_store::{
        inbox::{InboxEntry, InboxSummary},
        planning::PlanningSnapshot,
        view::FlowUpdate,
    },
    projection::flow::FlowTranscript,
};

/// Durable instance state at one committed cursor. Live resources and client drafts are observed separately.
#[derive(Clone, Debug)]
pub struct FlowViewSnapshot {
    pub display_title: String,
    pub history: Option<Arc<crate::flow_store::branches::ConversationView>>,
    pub status: InstanceStatus,
    pub planning: PlanningSnapshot,
    /// Recorded acquisition intent, not an observation of checkout availability.
    pub workspace: Option<crate::flow_store::workspace::WorkspaceRequest>,
    /// Bodies for the selected page, with editable current-turn input first.
    pub pending: Vec<InboxEntry>,
    /// Complete body-free display order. Each delivery group retains its recorded queue order.
    pub pending_order: Vec<InboxSummary>,
    /// Normalized offset into `pending_order`; clamped when the queue shrinks.
    pub pending_offset: usize,
}

/// Body page size for conversation views; it does not limit queue acceptance or consumption.
pub const PENDING_VIEW_PAGE_SIZE: usize = 32;

/// Conversation and state from the same committed boundary, shared by all kinds of flow views.
pub struct FlowView {
    transcript: FlowTranscript,
    snapshot: Option<Arc<FlowViewSnapshot>>,
}

impl FlowView {
    pub(crate) fn from_archive(
        transcript: FlowTranscript,
        execution: atman_rt::FlowInstance<crate::event::AtmanUuid>,
    ) -> io::Result<Self> {
        let cursor = transcript
            .cursor()
            .cloned()
            .ok_or_else(|| io::Error::other("archive has no committed cursor"))?;
        Ok(Self {
            transcript,
            snapshot: Some(Arc::new(FlowViewSnapshot {
                display_title: "History".into(),
                history: None,
                status: InstanceStatus {
                    execution,
                    goal: None,
                    completion: None,
                    cursor,
                },
                planning: Default::default(),
                workspace: None,
                pending: Vec::new(),
                pending_order: Vec::new(),
                pending_offset: 0,
            })),
        })
    }

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
    commits: watch::Receiver<FlowPosition>,
    limit: NonZeroUsize,
    view: FlowView,
    target: Option<Arc<FlowViewSnapshot>>,
    capture_pending: bool,
    pending_offset: usize,
    page_requested: bool,
    projection: Option<projection::ProjectionTask>,
    projection_pool: futures::executor::ThreadPool,
    progress: watch::Sender<Option<std::time::Instant>>,
}

impl FlowViewReader {
    /// Local reader activity, independent of execution state. Cancelled observations retain the same start time until a complete view or an error is returned.
    pub fn read_started_at(&self) -> Option<std::time::Instant> {
        *self.progress.borrow()
    }

    /// Subscribe before polling next/refresh to observe slow snapshot, read and projection work without replacing the previous document.
    pub fn subscribe_read_progress(&self) -> watch::Receiver<Option<std::time::Instant>> {
        self.progress.subscribe()
    }

    fn begin_read(&self) {
        if self.read_started_at().is_none() {
            self.progress.send_replace(Some(std::time::Instant::now()));
        }
    }
    /// The immutable destination, available before the first history page is read.
    pub fn address(&self) -> &crate::flow_store::FlowRef {
        self.worker.address()
    }

    /// Select a body page without consuming input or changing execution.
    pub fn set_pending_offset(&mut self, offset: usize) {
        if self.pending_offset != offset {
            self.pending_offset = offset;
            self.target = None;
            self.capture_pending = true;
            self.page_requested = true;
        }
    }
    /// A completed projection, or None while an interrupted read has only partially caught up.
    pub fn view(&self) -> Option<&FlowView> {
        self.view.snapshot.as_ref().and_then(|snapshot| {
            (self.view.transcript.cursor() == Some(&snapshot.status.cursor)).then_some(&self.view)
        })
    }

    /// Await a changed committed boundary or a requested body page. Cancellation preserves both the catch-up position and an observed notification that has not yet been read.
    pub async fn next(&mut self) -> io::Result<&FlowView> {
        let result = self.read_next().await;
        self.progress.send_replace(None);
        result?;
        Ok(&self.view)
    }

    async fn read_next(&mut self) -> io::Result<()> {
        loop {
            if self.target.is_none() {
                if !self.capture_pending {
                    self.commits.changed().await.map_err(|_| {
                        self.worker.storage_error(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "flow commit source closed",
                        ))
                    })?;
                    self.capture_pending = true;
                }
                self.begin_read();
                self.commits.borrow_and_update();
                self.target = Some(Arc::new(
                    self.worker.view_snapshot_at(self.pending_offset).await?,
                ));
                self.capture_pending = false;
            }
            if !self.page_requested
                && self.view().is_some_and(|view| {
                    view.snapshot().status.cursor == self.target.as_ref().unwrap().status.cursor
                        && view.snapshot().pending_offset
                            == self.target.as_ref().unwrap().pending_offset
                })
            {
                self.target = None;
                self.progress.send_replace(None);
                continue;
            }
            self.begin_read();
            self.catch_up().await?;
            return Ok(());
        }
    }

    /// Capture current committed state without waiting for a notification, including after host quiescence. Repeated refreshes retain existing record identities.
    pub async fn refresh(&mut self) -> io::Result<&FlowView> {
        self.begin_read();
        let result = self.read_current().await;
        self.progress.send_replace(None);
        result?;
        Ok(&self.view)
    }

    async fn read_current(&mut self) -> io::Result<()> {
        self.capture_pending = true;
        self.commits.borrow_and_update();
        self.target = Some(Arc::new(
            self.worker.view_snapshot_at(self.pending_offset).await?,
        ));
        self.capture_pending = false;
        self.catch_up().await?;
        Ok(())
    }

    async fn catch_up(&mut self) -> io::Result<()> {
        self.finish_projection().await?;
        let selection = self
            .target
            .as_ref()
            .expect("captured view boundary")
            .history
            .clone();
        if self.view.transcript.history_head()
            != selection.as_ref().map(|selection| selection.head())
        {
            let retired = std::mem::replace(
                &mut self.view.transcript,
                FlowTranscript::new(self.worker.address().clone()),
            );
            self.projection_pool.spawn_ok(async move {
                drop(retired);
            });
        }
        self.view.transcript.bind_selection(selection)?;
        let target = self
            .target
            .as_ref()
            .expect("captured view boundary")
            .status
            .cursor
            .clone();
        while self.view.transcript.cursor() != Some(&target) {
            let after = self.view.transcript.cursor().cloned();
            let remaining = target
                .sequence()
                .checked_sub(after.as_ref().map_or(0, |c| c.sequence()))
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
            let transcript = std::mem::replace(
                &mut self.view.transcript,
                FlowTranscript::new(self.worker.address().clone()),
            );
            self.projection = Some(projection::ProjectionTask::start(
                &self.projection_pool,
                transcript,
                page.batches,
            ));
            self.finish_projection().await?;
            // Long histories release the owner between pages and cooperate with other UI work.
            tokio::task::yield_now().await;
        }
        self.view.snapshot = self.target.take();
        self.pending_offset = self.view.snapshot().pending_offset;
        self.page_requested = false;
        Ok(())
    }

    async fn finish_projection(&mut self) -> io::Result<()> {
        let Some(task) = self.projection.as_mut() else {
            return Ok(());
        };
        let projected = task.finish().await;
        self.projection = None;
        self.view.transcript = projected.transcript;
        projected.result
    }
}

struct Feed {
    worker: FlowWorker,
    commits: watch::Receiver<FlowPosition>,
    initial: Option<FlowPage<FlowUpdate>>,
    after: Option<FlowPosition>,
    limit: NonZeroUsize,
    more: bool,
}

impl FlowWorker {
    /// Capture the first pending body page and complete queue metadata at one cursor.
    pub async fn view_snapshot(&self) -> io::Result<FlowViewSnapshot> {
        self.view_snapshot_at(0).await
    }

    /// Capture a pending body page without consuming messages. The offset is rounded to a page boundary and clamped to the last page.
    pub async fn view_snapshot_at(&self, offset: usize) -> io::Result<FlowViewSnapshot> {
        self.request(move |store| {
            let mut pending_order: Vec<_> = store.pending_input_entries().cloned().collect();
            pending_order.sort_by_key(|input| {
                !(input.envelope.request.is_editable_user_input()
                    && input.envelope.request.delivery
                        == crate::flow_store::inbox::DeliveryMode::AtBoundary)
            });
            let pending_offset = (offset / PENDING_VIEW_PAGE_SIZE)
                .min(pending_order.len().saturating_sub(1) / PENDING_VIEW_PAGE_SIZE)
                * PENDING_VIEW_PAGE_SIZE;
            let ids: Vec<_> = pending_order
                .iter()
                .skip(pending_offset)
                .take(PENDING_VIEW_PAGE_SIZE)
                .map(|input| input.envelope.id)
                .collect();
            let pending = ids
                .into_iter()
                .map(|id| {
                    store
                        .inbox_entry(id)?
                        .ok_or_else(|| io::Error::other("pending input disappeared"))
                })
                .collect::<io::Result<Vec<_>>>()?;
            Ok(FlowViewSnapshot {
                display_title: store.display_title(),
                history: store.conversation_view()?.map(Arc::new),
                status: InstanceStatus::from_store(store),
                planning: store.planning().clone(),
                workspace: store.workspace_request().cloned(),
                pending,
                pending_order,
                pending_offset,
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
            pending_offset: 0,
            page_requested: false,
            projection: None,
            projection_pool: projection::workers(),
            progress: watch::channel(None).0,
        })
    }

    /// Read an exclusive, instance-bound cursor through the storage owner. Pages preserve commit order and never start, resume, or consume flow work.
    pub async fn read_updates(
        &self,
        after: Option<FlowPosition>,
        limit: NonZeroUsize,
    ) -> io::Result<FlowPage<FlowUpdate>> {
        self.request(move |store| store.read_updates(after.as_ref(), limit))
            .await
    }

    /// Establish commit observation and validate the initial page before returning the stream. Catch-up reads and later notifications use one cursor; dropping the reader leaves execution untouched.
    pub async fn subscribe_updates(
        &self,
        after: Option<FlowPosition>,
        limit: NonZeroUsize,
    ) -> io::Result<BoxStream<'static, io::Result<FlowPage<FlowUpdate>>>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
            SessionDescriptor,
            inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity},
        },
        source_program::RevisionBundle,
    };

    #[tokio::test]
    async fn queued_projection_survives_cancelled_polls_and_page_recapture_without_consuming_input()
    {
        let directory = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(directory.path()).unwrap();
        let id = uuid::Uuid::now_v7();
        let mut session = root
            .create_session(SessionDescriptor {
                id,
                project: None,
                title: "Projection".into(),
                configuration: Default::default(),
            })
            .unwrap();
        let address = FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let mut store = session
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: uuid::Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        store
            .transition(store.state().revision(), FlowTransition::Activated)
            .unwrap();
        let worker = FlowWorker::start(store).await.unwrap();
        let mut reader = worker
            .subscribe_view(NonZeroUsize::new(2).unwrap())
            .await
            .unwrap();
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .unwrap();
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        pool.spawn_ok(async move {
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        entered.await.unwrap();
        reader.projection_pool = pool;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), reader.next())
                .await
                .is_err()
        );
        assert!(reader.projection.is_some());
        assert!(reader.view().is_none());
        let started = reader.read_started_at().unwrap();
        let large = "消息正文 ".repeat(100_000);
        for index in 0..33 {
            worker
                .accept_input(
                    DeliveryRequest {
                        expected_head: None,
                        request_id: uuid::Uuid::now_v7(),
                        sender: SenderIdentity::User(uuid::Uuid::nil()),
                        recipient: address.clone(),
                        delivery: DeliveryMode::NextTurn,
                        cause: None,
                        correlation: None,
                        scope: None,
                        payload: InboxPayload::Interjection {
                            text: if index == 32 {
                                large.clone()
                            } else {
                                index.to_string()
                            },
                        },
                    },
                    |_| Ok(()),
                )
                .await
                .unwrap();
        }
        let tip = worker.status().await.unwrap().cursor;
        reader.set_pending_offset(32);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), reader.next())
                .await
                .is_err()
        );
        assert!(reader.projection.is_some());
        assert_eq!(reader.read_started_at(), Some(started));
        release.send(()).unwrap();
        let view = tokio::time::timeout(std::time::Duration::from_secs(5), reader.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.transcript().cursor(), Some(&tip));
        assert_eq!(view.snapshot().status.cursor, tip);
        assert_eq!(view.snapshot().pending_offset, 32);
        assert_eq!(view.snapshot().pending.len(), 1);
        assert!(
            matches!(&view.snapshot().pending[0].envelope.request.payload,
            InboxPayload::Interjection { text } if text == &large)
        );
        assert_eq!(worker.snapshot().await.unwrap().pending.len(), 33);
        assert!(reader.read_started_at().is_none());
        drop(reader);
        let mut closing = worker
            .subscribe_view(NonZeroUsize::new(2).unwrap())
            .await
            .unwrap();
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .unwrap();
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        pool.spawn_ok(async move {
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        entered.await.unwrap();
        closing.projection_pool = pool;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), closing.next())
                .await
                .is_err()
        );
        drop(closing);
        tokio::time::timeout(std::time::Duration::from_secs(1), worker.shutdown())
            .await
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
    }
}
