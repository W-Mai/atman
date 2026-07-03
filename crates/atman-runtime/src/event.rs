use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct FlowRunId(pub Uuid);

impl FlowRunId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for FlowRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct TurnId(pub Uuid);

impl TurnId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    FlowStart {
        run_id: FlowRunId,
        flow_name: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
    FlowEnd {
        run_id: FlowRunId,
        flow_name: String,
        status: FlowStatus,
        ts: chrono::DateTime<chrono::Utc>,
    },
    LlmCall {
        model: String,
        provider: String,
        usage: crate::provider::TokenUsage,
        wallclock_ms: u64,
        status: LlmCallStatus,
        ts: chrono::DateTime<chrono::Utc>,
    },
    TurnStart {
        turn_id: TurnId,
        ts: chrono::DateTime<chrono::Utc>,
    },
    TurnEnd {
        turn_id: TurnId,
        ts: chrono::DateTime<chrono::Utc>,
    },
    UserMsg {
        turn_id: TurnId,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    AssistantMsg {
        turn_id: TurnId,
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ToolResultMsg {
        turn_id: TurnId,
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    SystemMsg {
        turn_id: TurnId,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    UserInject {
        turn_id: TurnId,
        injection: crate::injection::Injection,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ContentFilterHit {
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        provider: String,
        model: String,
        category: String,
        action: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowStatus {
    Ok,
    Errored { message: String },
}

impl FlowStatus {
    pub fn errored(msg: impl Into<String>) -> Self {
        Self::Errored {
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmCallStatus {
    Ok,
    Errored { message: String },
}

impl LlmCallStatus {
    pub fn errored(msg: impl Into<String>) -> Self {
        Self::Errored {
            message: msg.into(),
        }
    }
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
    forwarder: Option<mpsc::UnboundedSender<Event>>,
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_forwarder(mut self, tx: mpsc::UnboundedSender<Event>) -> Self {
        self.forwarder = Some(tx);
        self
    }

    pub fn emit(&self, event: Event) {
        if let Some(tx) = &self.forwarder {
            let _ = tx.send(event.clone());
        }
        self.events.lock().expect("event sink poisoned").push(event);
    }

    pub fn drain(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().expect("event sink poisoned"))
    }

    pub fn snapshot(&self) -> Vec<Event> {
        self.events.lock().expect("event sink poisoned").clone()
    }
}
