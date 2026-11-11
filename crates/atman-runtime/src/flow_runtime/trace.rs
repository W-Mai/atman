//! Scoped event capture drained by the existing flow storage owner.

use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use atman_rt::TurnLease;
use futures::task::AtomicWaker;
use tokio::sync::Notify;

use super::FlowWorker;
use crate::event::{AtmanUuid, Event, EventSink};
use crate::flow_store::{
    FlowStore,
    trace::{ExecutionEvent, ExecutionEventBatch, ExecutionScope},
};

#[derive(Default)]
pub(super) struct TraceBuffer {
    pending: Mutex<Vec<PendingEvent>>,
    pub wake: Notify,
    closed: AtomicBool,
    capacity: AtomicWaker,
}

// Apply backpressure at VM suspension boundaries, without dropping diagnostics
// or blocking executor threads on disk. This is a transport batch threshold,
// not an execution limit; one cooperative quantum may exceed it.
const PENDING_BATCH: usize = 256;

struct PendingEvent {
    event: ExecutionEvent,
    redactor: Option<Arc<crate::redact::Redactor>>,
}

impl TraceBuffer {
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.capacity.wake();
    }

    pub fn poll_capacity(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.pending.lock().unwrap().len() < PENDING_BATCH {
            return Poll::Ready(());
        }
        self.capacity.register(cx.waker());
        if self.closed.load(Ordering::Acquire) || self.pending.lock().unwrap().len() < PENDING_BATCH
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    pub fn bind(self: &Arc<Self>, sink: EventSink, scope: ExecutionScope) -> EventSink {
        let buffer = self.clone();
        let redactor = sink.redactor();
        sink.with_observer(Arc::new(move |envelope| {
            if buffer.closed.load(Ordering::Acquire) || !scope.accepts(&envelope.event) {
                return;
            }
            let mut pending = buffer.pending.lock().unwrap();
            if buffer.closed.load(Ordering::Acquire) {
                return;
            }
            pending.push(PendingEvent {
                event: ExecutionEvent {
                    scope: scope.clone(),
                    sequence: envelope.seq,
                    timestamp: envelope.ts,
                    event: envelope.event.clone(),
                },
                redactor: redactor.clone(),
            });
            buffer.wake.notify_one();
        }))
    }

    pub fn flush(&self, store: &mut FlowStore) -> io::Result<()> {
        if store.transient_frozen() {
            return Ok(());
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        let events = pending
            .into_iter()
            .map(|mut pending| {
                pending.event.event = redact(&pending.event.event, pending.redactor.as_deref())?;
                Ok(pending.event)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let result = if events.is_empty() {
            Ok(())
        } else {
            store.append_execution_events(events).map(|_| ())
        };
        self.capacity.wake();
        match result {
            Err(error) if crate::flow_store::transient::is_capacity_error(&error) => {
                store.observe_retention_limit()
            }
            result => result,
        }
    }
}

fn redact(event: &Event, redactor: Option<&crate::redact::Redactor>) -> io::Result<Event> {
    let Some(redactor) = redactor else {
        return Ok(event.clone());
    };
    let mut value = serde_json::to_value(event).map_err(io::Error::other)?;
    redactor.redact_json(&mut value);
    serde_json::from_value(value).map_err(io::Error::other)
}

pub(super) struct TraceOwner(pub Arc<TraceBuffer>);

impl Drop for TraceOwner {
    fn drop(&mut self) {
        self.0.close();
        self.0.pending.lock().unwrap().clear();
    }
}

impl FlowWorker {
    pub(crate) fn execution_sink(
        &self,
        lease: TurnLease<AtmanUuid>,
        source: &EventSink,
    ) -> EventSink {
        let mut sink = self.context().sink().clone();
        if let Some(redactor) = source.redactor() {
            sink = sink.with_redactor(redactor);
        }
        self.trace.bind(sink, ExecutionScope::Invocation(lease))
    }

    /// Read committed execution diagnostics; observation does not start or resume an instance.
    pub async fn execution_events(&self) -> io::Result<Vec<ExecutionEventBatch>> {
        self.request(|store| Ok(store.execution_events().to_vec()))
            .await
    }
}
