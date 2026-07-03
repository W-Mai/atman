use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::event::{Event, EventSink, FlowRunId, TurnId};
use crate::event_writer::EventWriter;
use crate::injection::{Injection, InjectionId, InjectionState};
use crate::message::{Message, MessageRole};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub struct Session {
    id: SessionId,
    dir: PathBuf,
    writer: Option<EventWriter>,
    sink: EventSink,
    messages: Mutex<Vec<Message>>,
    current_turn: Mutex<Option<TurnId>>,
    injection_queue: Mutex<Vec<Injection>>,
    flow_cancel: Mutex<CancellationToken>,
}

impl Session {
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let id = SessionId::now();
        let dir = root.as_ref().join("sessions").join(id.to_string());
        let writer = EventWriter::spawn(&dir)?;
        let sink = EventSink::new().with_forwarder(writer.sender());
        Ok(Self {
            id,
            dir,
            writer: Some(writer),
            sink,
            messages: Mutex::new(Vec::new()),
            current_turn: Mutex::new(None),
            injection_queue: Mutex::new(Vec::new()),
            flow_cancel: Mutex::new(CancellationToken::new()),
        })
    }

    pub fn open_ephemeral() -> Self {
        Self {
            id: SessionId::now(),
            dir: PathBuf::new(),
            writer: None,
            sink: EventSink::new(),
            messages: Mutex::new(Vec::new()),
            current_turn: Mutex::new(None),
            injection_queue: Mutex::new(Vec::new()),
            flow_cancel: Mutex::new(CancellationToken::new()),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn events_path(&self) -> Option<&Path> {
        self.writer.as_ref().map(|w| w.events_path())
    }

    pub fn sink(&self) -> &EventSink {
        &self.sink
    }

    /// Single-writer append. Emits the matching event before the in-memory push
    /// so events.jsonl remains the authority (§I5).
    pub fn append_message(&self, msg: Message, flow_run_id: Option<FlowRunId>) {
        let ts = chrono::Utc::now();
        let event = match msg.role {
            MessageRole::User => Event::UserMsg {
                turn_id: msg.turn_id.clone(),
                message: msg.clone(),
                ts,
            },
            MessageRole::Assistant => Event::AssistantMsg {
                turn_id: msg.turn_id.clone(),
                flow_run_id,
                message: msg.clone(),
                ts,
            },
            MessageRole::Tool => Event::ToolResultMsg {
                turn_id: msg.turn_id.clone(),
                flow_run_id,
                message: msg.clone(),
                ts,
            },
            MessageRole::System => Event::SystemMsg {
                turn_id: msg.turn_id.clone(),
                message: msg.clone(),
                ts,
            },
        };
        self.sink.emit(event);
        self.messages.lock().unwrap().push(msg);
    }

    pub fn messages(&self) -> Vec<Message> {
        self.messages.lock().unwrap().clone()
    }

    pub fn message_count(&self) -> usize {
        self.messages.lock().unwrap().len()
    }

    pub fn begin_turn(&self, user_msg: Message) -> TurnId {
        let turn_id = user_msg.turn_id.clone();
        *self.current_turn.lock().unwrap() = Some(turn_id.clone());
        *self.flow_cancel.lock().unwrap() = CancellationToken::new();
        self.sink.emit(Event::TurnStart {
            turn_id: turn_id.clone(),
            ts: chrono::Utc::now(),
        });
        self.append_message(user_msg, None);
        turn_id
    }

    pub fn end_turn(&self) {
        let turn_id = self.current_turn.lock().unwrap().take();
        if let Some(turn_id) = turn_id {
            let now = chrono::Utc::now();
            let mut q = self.injection_queue.lock().unwrap();
            for inj in q.iter_mut() {
                if inj.state == InjectionState::Pending && inj.turn_id == turn_id {
                    inj.state = InjectionState::Cancelled;
                }
            }
            drop(q);
            self.sink.emit(Event::TurnEnd { turn_id, ts: now });
        }
    }

    pub fn current_turn(&self) -> Option<TurnId> {
        self.current_turn.lock().unwrap().clone()
    }

    pub fn enqueue_injection(&self, text: impl Into<String>) -> Result<InjectionId, EnqueueError> {
        let turn_id = self
            .current_turn
            .lock()
            .unwrap()
            .clone()
            .ok_or(EnqueueError::NoActiveTurn)?;
        let inj = Injection::new_pending(turn_id.clone(), text);
        let id = inj.id.clone();
        self.sink.emit(Event::UserInject {
            turn_id,
            injection: inj.clone(),
            ts: inj.created_at,
        });
        self.injection_queue.lock().unwrap().push(inj);
        Ok(id)
    }

    /// Drain all Pending injections for `turn_id`. Marks them Injected.
    /// Returns them in creation order.
    pub fn drain_injections(&self, turn_id: &TurnId) -> Vec<Injection> {
        let mut q = self.injection_queue.lock().unwrap();
        let mut out = Vec::new();
        for inj in q.iter_mut() {
            if inj.state == InjectionState::Pending && inj.turn_id == *turn_id {
                inj.state = InjectionState::Injected;
                out.push(inj.clone());
            }
        }
        out
    }

    pub fn list_pending_injections(&self) -> Vec<Injection> {
        self.injection_queue
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.state == InjectionState::Pending)
            .cloned()
            .collect()
    }

    pub fn cancel_flow(&self) {
        self.flow_cancel.lock().unwrap().cancel();
    }

    pub fn flow_cancel_token(&self) -> CancellationToken {
        self.flow_cancel.lock().unwrap().clone()
    }

    pub async fn shutdown(mut self) {
        if let Some(writer) = self.writer.take() {
            writer.shutdown().await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("enqueue_injection called with no active turn")]
    NoActiveTurn,
}
