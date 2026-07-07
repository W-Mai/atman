use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::event::{Event, EventSink, FlowRunId, TurnId};
use crate::event_writer::EventWriter;
use crate::injection::{Injection, InjectionId, InjectionState};
use crate::message::{Message, MessageRole};
use crate::stream::StreamFrame;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn parse(s: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(s).map(Self)
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
    injection_tx: broadcast::Sender<Injection>,
    stream_tx: broadcast::Sender<StreamFrame>,
    flow_cancel: Mutex<CancellationToken>,
    context_watch: watch::Sender<ContextSnapshot>,
    goal_watch: watch::Sender<Option<String>>,
    attach_watch: watch::Sender<usize>,
    todos_watch: watch::Sender<Vec<crate::memory::todo::Todo>>,
    streamed_this_turn: std::sync::atomic::AtomicBool,
    last_image_user_msg: Mutex<Option<LastImageUserMsg>>,
}

type ImagePart = (usize, String);

#[derive(Debug, Clone)]
struct LastImageUserMsg {
    message_seq: u64,
    images: Vec<ImagePart>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextSnapshot {
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub mcp_ok: u16,
    pub mcp_total: u16,
    pub memory_recent_count: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionOpenError {
    #[error("invalid session id `{sid}` (want a UUID)")]
    InvalidId { sid: String },
    #[error("session `{sid}` not found at {}", dir.display())]
    NotFound { sid: String, dir: PathBuf },
    #[error("session writer init: {0}")]
    WriterInit(#[source] std::io::Error),
    #[error("replay {}: {source}", path.display())]
    Replay {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn load_goal(dir: &Path) -> Option<String> {
    if dir.as_os_str().is_empty() {
        return None;
    }
    let store = crate::memory::goal::GoalStore::at(dir);
    match store.get() {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    Message {
        message: Message,
        flow_run_id: Option<String>,
    },
    FlowGraph {
        run_id: String,
        flow_name: String,
        graph: crate::nodegraph::FlowGraph,
    },
    FlowStart {
        run_id: String,
        flow_name: String,
        parent_run_id: Option<String>,
        parent_node_id: Option<String>,
    },
    FlowNodeStart {
        run_id: String,
        node_id: String,
        kind: crate::nodegraph::NodeKind,
        label: String,
        parent_node_id: Option<String>,
    },
    FlowNodeEnd {
        run_id: String,
        node_id: String,
        status: crate::event::FlowNodeStatus,
        output_preview: Option<String>,
    },
    ToolNode {
        run_id: String,
        parent_node_id: String,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
    },
    FlowDone {
        run_id: String,
        ok: bool,
    },
}

fn replay_messages_from(path: &Path) -> Result<Vec<Message>, SessionOpenError> {
    let entries = replay_transcript_from(path)?;
    let mut out = Vec::new();
    for entry in entries {
        if let TranscriptEntry::Message { message, .. } = entry {
            out.push(message);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct AttachmentPatch {
    part_index: usize,
    file_basename: String,
    reason: String,
}

fn parse_json_lines(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.is_empty() {
                None
            } else {
                serde_json::from_str::<serde_json::Value>(t).ok()
            }
        })
        .collect()
}

fn collect_attachment_patches(
    values: &[serde_json::Value],
) -> std::collections::HashMap<u64, Vec<AttachmentPatch>> {
    let mut map: std::collections::HashMap<u64, Vec<AttachmentPatch>> =
        std::collections::HashMap::new();
    for v in values {
        if v["type"].as_str() == Some("attachment_degraded") {
            let Some(msg_seq) = v["message_seq"].as_u64() else {
                continue;
            };
            let Some(part_index) = v["part_index"].as_u64() else {
                continue;
            };
            let file_basename = v["file_basename"].as_str().unwrap_or("").to_string();
            let reason = v["reason"].as_str().unwrap_or("degraded").to_string();
            map.entry(msg_seq).or_default().push(AttachmentPatch {
                part_index: part_index as usize,
                file_basename,
                reason,
            });
        }
    }
    map
}

fn apply_attachment_patches(msg: &mut Message, patches: &[AttachmentPatch]) {
    for p in patches {
        if let Some(part) = msg.parts.get_mut(p.part_index) {
            *part = crate::message::MessagePart::Text {
                text: format!(
                    "[attachment unavailable: {} — {}]",
                    p.file_basename, p.reason
                ),
            };
        }
    }
}

pub fn replay_transcript_from(path: &Path) -> Result<Vec<TranscriptEntry>, SessionOpenError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let values = parse_json_lines(&text);
    let patches = collect_attachment_patches(&values);
    let mut out = Vec::new();
    for v in &values {
        let ty = v["type"].as_str().unwrap_or("");
        match ty {
            "user_msg" | "assistant_msg" | "tool_result_msg" | "system_msg" => {
                if let Some(m) = v.get("message")
                    && let Ok(mut msg) = serde_json::from_value::<Message>(m.clone())
                {
                    if let Some(seq) = v["seq"].as_u64()
                        && let Some(ps) = patches.get(&seq)
                    {
                        apply_attachment_patches(&mut msg, ps);
                    }
                    let flow_run_id = v["flow_run_id"].as_str().map(String::from);
                    out.push(TranscriptEntry::Message {
                        message: msg,
                        flow_run_id,
                    });
                }
            }
            "flow_graph" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let flow_name = v
                    .get("graph")
                    .and_then(|g| g["flow_name"].as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(g) = v.get("graph")
                    && let Ok(graph) =
                        serde_json::from_value::<crate::nodegraph::FlowGraph>(g.clone())
                {
                    out.push(TranscriptEntry::FlowGraph {
                        run_id,
                        flow_name,
                        graph,
                    });
                }
            }
            "flow_start" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let flow_name = v["flow_name"].as_str().unwrap_or("").to_string();
                let parent_run_id = v["parent_run_id"].as_str().map(String::from);
                let parent_node_id = v["parent_node_id"].as_str().map(String::from);
                out.push(TranscriptEntry::FlowStart {
                    run_id,
                    flow_name,
                    parent_run_id,
                    parent_node_id,
                });
            }
            "flow_node_start" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let node_id = v["node_id"].as_str().unwrap_or("").to_string();
                let label = v["label"].as_str().unwrap_or(&node_id).to_string();
                let parent_node_id = v["parent_node_id"].as_str().map(String::from);
                let kind = v
                    .get("kind")
                    .and_then(|k| serde_json::from_value(k.clone()).ok())
                    .unwrap_or(crate::nodegraph::NodeKind::UserConfirm);
                out.push(TranscriptEntry::FlowNodeStart {
                    run_id,
                    node_id,
                    kind,
                    label,
                    parent_node_id,
                });
            }
            "flow_node_end" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let node_id = v["node_id"].as_str().unwrap_or("").to_string();
                let status: crate::event::FlowNodeStatus = v
                    .get("status")
                    .and_then(|s| serde_json::from_value(s.clone()).ok())
                    .unwrap_or(crate::event::FlowNodeStatus::Ok);
                let output_preview = v["output_preview"].as_str().map(String::from);
                out.push(TranscriptEntry::FlowNodeEnd {
                    run_id,
                    node_id,
                    status,
                    output_preview,
                });
            }
            "tool_node" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let parent_node_id = v["parent_node_id"].as_str().unwrap_or("").to_string();
                let tool_use_id = v["tool_use_id"].as_str().unwrap_or("").to_string();
                let tool_name = v["tool_name"].as_str().unwrap_or("").to_string();
                let args_preview = v["args_preview"].as_str().unwrap_or("").to_string();
                out.push(TranscriptEntry::ToolNode {
                    run_id,
                    parent_node_id,
                    tool_use_id,
                    tool_name,
                    args_preview,
                });
            }
            "flow_end" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let ok = v["status"]["kind"].as_str() == Some("ok");
                out.push(TranscriptEntry::FlowDone { run_id, ok });
            }
            _ => {}
        }
    }
    Ok(out)
}

impl Session {
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::open_with_redactor(root, None)
    }

    pub fn open_with_redactor(
        root: impl AsRef<Path>,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
    ) -> std::io::Result<Self> {
        let id = SessionId::now();
        let dir = root.as_ref().join("sessions").join(id.to_string());
        let writer = EventWriter::spawn_with(&dir, redactor.clone())?;
        let mut sink = EventSink::new().with_forwarder(writer.sender());
        if let Some(r) = redactor {
            sink = sink.with_redactor(r);
        }
        let (injection_tx, _) = broadcast::channel(32);
        let (stream_tx, _) = broadcast::channel(256);
        let (context_watch, _) = watch::channel(ContextSnapshot::default());
        let (goal_watch, _) = watch::channel(None);
        let (attach_watch, _) = watch::channel(0);
        let (todos_watch, _) = watch::channel(Vec::new());
        Ok(Self {
            id,
            dir,
            writer: Some(writer),
            sink,
            messages: Mutex::new(Vec::new()),
            current_turn: Mutex::new(None),
            injection_queue: Mutex::new(Vec::new()),
            injection_tx,
            stream_tx,
            flow_cancel: Mutex::new(CancellationToken::new()),
            context_watch,
            goal_watch,
            attach_watch,
            todos_watch,
            streamed_this_turn: std::sync::atomic::AtomicBool::new(false),
            last_image_user_msg: Mutex::new(None),
        })
    }

    pub fn open_existing(root: impl AsRef<Path>, sid: &str) -> Result<Self, SessionOpenError> {
        Self::open_existing_with_redactor(root, sid, None)
    }

    pub fn open_existing_with_redactor(
        root: impl AsRef<Path>,
        sid: &str,
        redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
    ) -> Result<Self, SessionOpenError> {
        let id = SessionId::parse(sid).map_err(|_| SessionOpenError::InvalidId {
            sid: sid.to_string(),
        })?;
        let dir = root.as_ref().join("sessions").join(id.to_string());
        if !dir.exists() {
            return Err(SessionOpenError::NotFound {
                sid: sid.to_string(),
                dir: dir.clone(),
            });
        }
        let writer = EventWriter::spawn_with(&dir, redactor.clone())
            .map_err(SessionOpenError::WriterInit)?;
        let mut sink = EventSink::new().with_forwarder(writer.sender());
        if let Some(r) = redactor {
            sink = sink.with_redactor(r);
        }
        let messages = replay_messages_from(&dir.join("events.jsonl"))?;
        let initial_goal = load_goal(&dir);
        let (injection_tx, _) = broadcast::channel(32);
        let (stream_tx, _) = broadcast::channel(256);
        let (context_watch, _) = watch::channel(ContextSnapshot::default());
        let (goal_watch, _) = watch::channel(initial_goal);
        let (attach_watch, _) = watch::channel(0);
        let (todos_watch, _) = watch::channel(Vec::new());
        Ok(Self {
            id,
            dir,
            writer: Some(writer),
            sink,
            messages: Mutex::new(messages),
            current_turn: Mutex::new(None),
            injection_queue: Mutex::new(Vec::new()),
            injection_tx,
            stream_tx,
            flow_cancel: Mutex::new(CancellationToken::new()),
            context_watch,
            goal_watch,
            attach_watch,
            todos_watch,
            streamed_this_turn: std::sync::atomic::AtomicBool::new(false),
            last_image_user_msg: Mutex::new(None),
        })
    }

    pub fn open_ephemeral() -> Self {
        let (injection_tx, _) = broadcast::channel(32);
        let (stream_tx, _) = broadcast::channel(256);
        let (context_watch, _) = watch::channel(ContextSnapshot::default());
        let (goal_watch, _) = watch::channel(None);
        let (attach_watch, _) = watch::channel(0);
        let (todos_watch, _) = watch::channel(Vec::new());
        Self {
            id: SessionId::now(),
            dir: PathBuf::new(),
            writer: None,
            sink: EventSink::new(),
            messages: Mutex::new(Vec::new()),
            current_turn: Mutex::new(None),
            injection_queue: Mutex::new(Vec::new()),
            injection_tx,
            stream_tx,
            flow_cancel: Mutex::new(CancellationToken::new()),
            context_watch,
            goal_watch,
            attach_watch,
            todos_watch,
            streamed_this_turn: std::sync::atomic::AtomicBool::new(false),
            last_image_user_msg: Mutex::new(None),
        }
    }

    pub fn stream_tx(&self) -> broadcast::Sender<StreamFrame> {
        self.stream_tx.clone()
    }

    pub fn stream_subscribe(&self) -> broadcast::Receiver<StreamFrame> {
        self.stream_tx.subscribe()
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn transcript_replay(&self) -> Vec<TranscriptEntry> {
        let Some(path) = self.events_path() else {
            return Vec::new();
        };
        replay_transcript_from(path).unwrap_or_default()
    }

    pub fn events_path(&self) -> Option<&Path> {
        self.writer.as_ref().map(|w| w.events_path())
    }

    pub fn goal(&self) -> Option<String> {
        if let Some(cached) = self.goal_watch.borrow().clone() {
            return Some(cached);
        }
        load_goal(&self.dir)
    }

    pub fn subscribe_goal(&self) -> watch::Receiver<Option<String>> {
        self.goal_watch.subscribe()
    }

    pub fn subscribe_context(&self) -> watch::Receiver<ContextSnapshot> {
        self.context_watch.subscribe()
    }

    pub fn subscribe_attach(&self) -> watch::Receiver<usize> {
        self.attach_watch.subscribe()
    }

    pub fn set_goal(&self, goal: Option<String>) {
        let _ = self.goal_watch.send(goal);
    }

    pub fn set_attach_count(&self, count: usize) {
        let _ = self.attach_watch.send(count);
    }

    pub fn record_llm_call(&self, model: &str, tokens_in: u64, tokens_out: u64) {
        self.context_watch.send_modify(|snap| {
            snap.model = model.to_string();
            snap.tokens_in = snap.tokens_in.saturating_add(tokens_in);
            snap.tokens_out = snap.tokens_out.saturating_add(tokens_out);
        });
    }

    pub fn set_mcp_totals(&self, ok: u16, total: u16) {
        self.context_watch.send_modify(|snap| {
            snap.mcp_ok = ok;
            snap.mcp_total = total;
        });
    }

    pub fn set_memory_recent_count(&self, count: u16) {
        self.context_watch.send_modify(|snap| {
            snap.memory_recent_count = count;
        });
    }

    pub fn subscribe_todos(&self) -> watch::Receiver<Vec<crate::memory::todo::Todo>> {
        self.todos_watch.subscribe()
    }

    pub fn refresh_todos_from_store(&self) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        let store = crate::memory::todo::TodoStore::at(&self.dir);
        match tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current()
                .ok()
                .map(|h| h.block_on(store.list()))
        }) {
            Some(Ok(list)) => {
                let _ = self.todos_watch.send(list);
            }
            Some(Err(e)) => {
                eprintln!("[atman] refresh_todos_from_store: {e}");
            }
            None => {}
        }
    }

    pub async fn refresh_todos_from_store_async(&self) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        let store = crate::memory::todo::TodoStore::at(&self.dir);
        match store.list().await {
            Ok(list) => {
                let _ = self.todos_watch.send(list);
            }
            Err(e) => {
                eprintln!("[atman] refresh_todos_from_store_async: {e}");
            }
        }
    }

    pub fn sink(&self) -> &EventSink {
        &self.sink
    }

    /// Single-writer append. Emits the matching event before the in-memory push
    /// so events.jsonl remains the authority (§I5).
    pub fn append_message(&self, msg: Message, flow_run_id: Option<FlowRunId>) {
        let ts = chrono::Utc::now();
        let flow_run_id_str = flow_run_id.as_ref().map(|r| r.0.to_string());
        let event = match msg.role {
            MessageRole::User => Event::UserMsg {
                seq: 0,
                turn_id: msg.turn_id.clone(),
                message: msg.clone(),
                ts,
            },
            MessageRole::Assistant => {
                let _ = self
                    .stream_tx
                    .send(crate::stream::StreamFrame::AssistantMsg {
                        flow_run_id: flow_run_id_str.clone(),
                        message: msg.clone(),
                    });
                Event::AssistantMsg {
                    seq: 0,
                    turn_id: msg.turn_id.clone(),
                    flow_run_id,
                    message: msg.clone(),
                    ts,
                }
            }
            MessageRole::Tool => {
                let _ = self
                    .stream_tx
                    .send(crate::stream::StreamFrame::ToolResultMsg {
                        flow_run_id: flow_run_id_str.clone(),
                        message: msg.clone(),
                    });
                Event::ToolResultMsg {
                    seq: 0,
                    turn_id: msg.turn_id.clone(),
                    flow_run_id,
                    message: msg.clone(),
                    ts,
                }
            }
            MessageRole::System => Event::SystemMsg {
                seq: 0,
                turn_id: msg.turn_id.clone(),
                message: msg.clone(),
                ts,
            },
        };
        let seq = self.sink.emit_returning_seq(event);
        if matches!(msg.role, MessageRole::User) {
            let images: Vec<(usize, String)> = msg
                .parts
                .iter()
                .enumerate()
                .filter_map(|(i, p)| match p {
                    crate::message::MessagePart::Image { source } => {
                        let basename = match &source.data {
                            crate::message::ImageData::Path { path } => path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            crate::message::ImageData::Base64 { .. } => "base64".into(),
                        };
                        Some((i, basename))
                    }
                    _ => None,
                })
                .collect();
            if !images.is_empty() {
                *self.last_image_user_msg.lock().unwrap() = Some(LastImageUserMsg {
                    message_seq: seq,
                    images,
                });
            }
        }
        self.messages.lock().unwrap().push(msg);
    }

    pub fn record_attachment_degrade(&self, reason: &str) -> usize {
        let target = self.last_image_user_msg.lock().unwrap().take();
        let Some(entry) = target else {
            return 0;
        };
        let turn_id = self.current_turn.lock().unwrap().clone();
        let now = chrono::Utc::now();
        for (part_index, basename) in &entry.images {
            self.sink.emit(Event::AttachmentDegraded {
                seq: 0,
                turn_id: turn_id.clone(),
                flow_run_id: None,
                message_seq: entry.message_seq,
                part_index: *part_index,
                file_basename: basename.clone(),
                reason: reason.into(),
                ts: now,
            });
        }
        if let Ok(mut msgs) = self.messages.lock() {
            for m in msgs.iter_mut() {
                for (part_index, basename) in &entry.images {
                    if let Some(part) = m.parts.get_mut(*part_index)
                        && matches!(part, crate::message::MessagePart::Image { .. })
                    {
                        *part = crate::message::MessagePart::Text {
                            text: format!("[attachment unavailable: {basename} — {reason}]"),
                        };
                    }
                }
            }
        }
        entry.images.len()
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
            seq: 0,
            turn_id: turn_id.clone(),
            ts: chrono::Utc::now(),
        });
        self.append_message(user_msg, None);
        turn_id
    }

    pub fn mark_streamed(&self) {
        self.streamed_this_turn
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn take_streamed_flag(&self) -> bool {
        self.streamed_this_turn
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn end_turn(&self) {
        self.streamed_this_turn
            .store(false, std::sync::atomic::Ordering::Relaxed);
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
            self.sink.emit(Event::TurnEnd {
                seq: 0,
                turn_id,
                ts: now,
            });
        }
    }

    pub fn current_turn(&self) -> Option<TurnId> {
        self.current_turn.lock().unwrap().clone()
    }

    pub fn enqueue_injection(&self, text: impl Into<String>) -> Result<InjectionId, EnqueueError> {
        self.enqueue_injection_with_level(text, crate::injection::InjectionLevel::L1Nudge, None)
    }

    pub fn enqueue_injection_with_level(
        &self,
        text: impl Into<String>,
        level: crate::injection::InjectionLevel,
        redirect_target: Option<String>,
    ) -> Result<InjectionId, EnqueueError> {
        let turn_id = self
            .current_turn
            .lock()
            .unwrap()
            .clone()
            .ok_or(EnqueueError::NoActiveTurn)?;
        let inj = Injection::with_level(turn_id.clone(), text, level, redirect_target);
        let id = inj.id.clone();
        self.sink.emit(Event::UserInject {
            seq: 0,
            turn_id,
            injection: inj.clone(),
            ts: inj.created_at,
        });
        self.injection_queue.lock().unwrap().push(inj.clone());
        let _ = self.injection_tx.send(inj);
        Ok(id)
    }

    pub fn subscribe_injections(&self) -> broadcast::Receiver<Injection> {
        self.injection_tx.subscribe()
    }

    pub fn mark_injection_consumed(&self, id: &InjectionId) {
        let mut q = self.injection_queue.lock().unwrap();
        for inj in q.iter_mut() {
            if inj.id == *id && inj.state == InjectionState::Pending {
                inj.state = InjectionState::Injected;
                return;
            }
        }
    }

    pub fn peek_pending_l2_or_higher(&self, turn_id: &TurnId) -> Option<Injection> {
        let q = self.injection_queue.lock().unwrap();
        q.iter()
            .find(|i| {
                i.state == InjectionState::Pending
                    && i.turn_id == *turn_id
                    && !matches!(i.level, crate::injection::InjectionLevel::L1Nudge)
            })
            .cloned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_events(dir: &Path, lines: &[&str]) {
        let path = dir.join("events.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn replay_applies_attachment_degraded_patch() {
        let dir = TempDir::new().unwrap();
        let user_msg = r#"{"type":"user_msg","seq":5,"turn_id":"019f0000-0000-7000-0000-000000000001","message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/photo.png"}}},{"type":"text","text":"describe"}],"turn_id":"019f0000-0000-7000-0000-000000000001"},"ts":"2026-07-07T00:00:00Z"}"#;
        let degrade = r#"{"type":"attachment_degraded","seq":6,"turn_id":null,"flow_run_id":null,"message_seq":5,"part_index":0,"file_basename":"photo.png","reason":"image_too_large","ts":"2026-07-07T00:00:01Z"}"#;
        write_events(dir.path(), &[user_msg, degrade]);
        let entries = replay_transcript_from(&dir.path().join("events.jsonl")).unwrap();
        let msg = entries
            .into_iter()
            .find_map(|e| match e {
                TranscriptEntry::Message { message, .. } => Some(message),
                _ => None,
            })
            .unwrap();
        assert_eq!(msg.parts.len(), 2);
        match &msg.parts[0] {
            crate::message::MessagePart::Text { text } => {
                assert!(text.contains("photo.png"), "expected basename: {text}");
                assert!(text.contains("image_too_large"), "expected reason: {text}");
                assert!(text.starts_with("[attachment unavailable"));
            }
            other => panic!("expected Text stub, got {other:?}"),
        }
        assert!(matches!(
            msg.parts[1],
            crate::message::MessagePart::Text { .. }
        ));
    }

    #[test]
    fn replay_without_degraded_events_preserves_image_parts() {
        let dir = TempDir::new().unwrap();
        let user_msg = r#"{"type":"user_msg","seq":1,"turn_id":"019f0000-0000-7000-0000-000000000002","message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/x.png"}}}],"turn_id":"019f0000-0000-7000-0000-000000000002"},"ts":"2026-07-07T00:00:00Z"}"#;
        write_events(dir.path(), &[user_msg]);
        let entries = replay_transcript_from(&dir.path().join("events.jsonl")).unwrap();
        let msg = entries
            .into_iter()
            .find_map(|e| match e {
                TranscriptEntry::Message { message, .. } => Some(message),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            msg.parts[0],
            crate::message::MessagePart::Image { .. }
        ));
    }
}
