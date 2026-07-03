use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct FlowRunId(pub u64);

#[derive(Debug, Clone)]
pub enum Event {
    FlowStart {
        run_id: FlowRunId,
        flow_name: String,
    },
    FlowEnd {
        run_id: FlowRunId,
        flow_name: String,
        status: FlowStatus,
    },
}

#[derive(Debug, Clone)]
pub enum FlowStatus {
    Ok,
    Errored(String),
}

#[derive(Debug, Clone)]
pub enum NodeEvent {
    LlmChunk {
        text: String,
        cumulative_tokens: u64,
    },
    LlmDone {
        total_tokens: u64,
    },
    ToolStdoutLine {
        line: String,
        seq: u64,
    },
    ToolStderrLine {
        line: String,
        seq: u64,
    },
    ToolDone {
        exit: i32,
    },
}

pub struct Observable<T> {
    pub output: crate::tool::BoxFut<'static, Result<T, crate::error::RuntimeError>>,
    pub events: broadcast::Receiver<NodeEvent>,
    pub cancel: CancellationToken,
}

#[derive(Default, Clone)]
pub struct EventSink {
    events: Arc<Mutex<Vec<Event>>>,
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emit(&self, event: Event) {
        self.events.lock().expect("event sink poisoned").push(event);
    }

    pub fn drain(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().expect("event sink poisoned"))
    }

    pub fn snapshot(&self) -> Vec<Event> {
        self.events.lock().expect("event sink poisoned").clone()
    }
}
