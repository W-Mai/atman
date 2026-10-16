use std::{io, num::NonZeroUsize};

use futures::{StreamExt, stream::BoxStream};
use tokio::sync::watch;

use super::FlowWorker;
use crate::{
    event_log::journal::{CommitReceipt, JournalPage},
    flow_store::view::FlowUpdate,
};

struct Feed {
    worker: FlowWorker,
    commits: watch::Receiver<CommitReceipt>,
    initial: Option<JournalPage<FlowUpdate>>,
    after: Option<CommitReceipt>,
    limit: NonZeroUsize,
    more: bool,
}

impl FlowWorker {
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
