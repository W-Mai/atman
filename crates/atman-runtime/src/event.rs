use std::sync::{Arc, Mutex};

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
