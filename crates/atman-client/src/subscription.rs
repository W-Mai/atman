//! One source-bound durable observer. Observation lifetime does not own execution.

use std::collections::VecDeque;

use atman_proto::{
    client::FlowEventSubscription,
    flows::{FlowAddress, FlowCursor},
    responses::{FlowEventBatch, FlowEventPage},
};
use futures::{StreamExt, stream::BoxStream};

use crate::{ClientError, Result};

pub struct EventSubscription {
    source: FlowEventSubscription,
    pages: BoxStream<'static, Result<FlowEventPage>>,
    ready: VecDeque<FlowEventBatch>,
    terminal: bool,
}

impl EventSubscription {
    pub(crate) fn new(
        flow: FlowAddress,
        after: Option<FlowCursor>,
        pages: BoxStream<'static, Result<FlowEventPage>>,
    ) -> Self {
        Self {
            source: FlowEventSubscription::new(flow, after),
            pages,
            ready: VecDeque::new(),
            terminal: false,
        }
    }

    pub fn flow(&self) -> &FlowAddress {
        self.source.flow()
    }

    /// Last consumed batch, suitable for an explicit reconnect to the same Flow.
    pub fn last(&self) -> Option<&FlowCursor> {
        self.source.last()
    }

    pub async fn next(&mut self) -> Option<Result<FlowEventBatch>> {
        if self.terminal {
            return None;
        }
        loop {
            if let Some(batch) = self.ready.pop_front() {
                let next = Some(batch.receipt.clone());
                match self.source.ingest(FlowEventPage {
                    batches: vec![batch],
                    next,
                    has_more: false,
                }) {
                    Ok(mut fresh) => {
                        if let Some(batch) = fresh.pop() {
                            return Some(Ok(batch));
                        }
                        continue;
                    }
                    Err(error) => {
                        self.terminal = true;
                        return Some(Err(error.into()));
                    }
                }
            }
            let page = match self.pages.next().await? {
                Ok(page) => page,
                Err(error) => {
                    self.terminal = true;
                    return Some(Err(error));
                }
            };
            if let Err(error) = validate_page(self.source.flow(), &page) {
                self.terminal = true;
                return Some(Err(error));
            }
            self.ready.extend(page.batches);
        }
    }
}

pub(crate) fn validate_page(flow: &FlowAddress, page: &FlowEventPage) -> Result<()> {
    for cursor in page
        .batches
        .iter()
        .map(|batch| &batch.receipt)
        .chain(page.next.iter())
    {
        if cursor.journal_id != flow.instance_id
            || cursor.sequence == 0
            || cursor.digest.len() != 64
            || !cursor.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ClientError::EventSource);
        }
    }
    Ok(())
}
