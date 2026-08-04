use crate::error::RuntimeError;
use crate::event::{Event, FlowRunId, FlowStatus};
use crate::message::Message;
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct AgentSpawn;

#[derive(Debug, Clone)]
pub enum AgentRunStatus {
    Running {
        started_at: chrono::DateTime<chrono::Utc>,
    },
    Ok {
        ended_at: chrono::DateTime<chrono::Utc>,
        final_text: String,
    },
    Err {
        ended_at: chrono::DateTime<chrono::Utc>,
        message: String,
    },
    Killed {
        ended_at: chrono::DateTime<chrono::Utc>,
    },
}

impl AgentRunStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Running { .. } => "running",
            Self::Ok { .. } => "ok",
            Self::Err { .. } => "err",
            Self::Killed { .. } => "killed",
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    AssistantDone { text: String },
    Exited { status: AgentRunStatus },
}

pub struct AgentEntry {
    pub handle: String,
    pub goal: String,
    pub status: Arc<Mutex<AgentRunStatus>>,
    pub output: Arc<Mutex<String>>,
    pub cancel: tokio_util::sync::CancellationToken,
    pub stream_tx: tokio::sync::broadcast::Sender<AgentEvent>,
    pub messages: Arc<Mutex<Vec<Message>>>,
    pub iteration: Arc<std::sync::atomic::AtomicU64>,
    pub child_run_id: FlowRunId,
    pub model: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Per-FlowRun compaction lock. Bound to this entry's messages_handle so
    /// the sub-agent can compact its own segment independently of the root
    /// session's compaction.
    pub compact_lock: Arc<tokio::sync::Mutex<()>>,
}

impl crate::watch::Watchable for AgentEntry {
    fn watch_output(
        self: Arc<Self>,
        pattern: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::watch::WatchResult> + Send>>
    {
        let stream_tx = self.stream_tx.clone();
        let output = self.output.clone();
        Box::pin(async move {
            {
                let existing = output.lock().unwrap().clone();
                if existing.find(&pattern).is_some() {
                    return crate::watch::WatchResult::Matched {
                        row: None,
                        col: None,
                        text: existing,
                    };
                }
            }
            let mut rx = stream_tx.subscribe();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return crate::watch::WatchResult::Cancelled,
                    result = rx.recv() => match result {
                        Ok(AgentEvent::AssistantDone { text }) => {
                            if text.find(&pattern).is_some() {
                                return crate::watch::WatchResult::Matched {
                                    row: None,
                                    col: None,
                                    text,
                                };
                            }
                        }
                        Ok(AgentEvent::Exited { status }) => {
                            if let AgentRunStatus::Ok { final_text, .. } = &status {
                                if final_text.find(&pattern).is_some() {
                                    return crate::watch::WatchResult::Matched {
                                        row: None,
                                        col: None,
                                        text: final_text.clone(),
                                    };
                                }
                            }
                            return crate::watch::WatchResult::SourceExited;
                        }
                        Err(_) => return crate::watch::WatchResult::SourceExited,
                    }
                }
            }
        })
    }
}

#[derive(Default)]
pub struct AgentRegistry {
    entries: Mutex<std::collections::HashMap<String, Arc<AgentEntry>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_entry(
        &self,
        handle: String,
        goal: String,
        model: String,
        child_run_id: FlowRunId,
    ) -> Arc<AgentEntry> {
        let (stream_tx, _) = tokio::sync::broadcast::channel(64);
        let entry = Arc::new(AgentEntry {
            handle: handle.clone(),
            goal,
            status: Arc::new(Mutex::new(AgentRunStatus::Running {
                started_at: chrono::Utc::now(),
            })),
            output: Arc::new(Mutex::new(String::new())),
            cancel: tokio_util::sync::CancellationToken::new(),
            stream_tx,
            messages: Arc::new(Mutex::new(Vec::new())),
            iteration: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            child_run_id,
            model,
            started_at: chrono::Utc::now(),
            compact_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        self.entries
            .lock()
            .unwrap()
            .insert(handle, Arc::clone(&entry));
        entry
    }

    pub fn lookup(&self, handle: &str) -> Result<Arc<AgentEntry>, RuntimeError> {
        self.entries
            .lock()
            .unwrap()
            .get(handle)
            .map(Arc::clone)
            .ok_or_else(|| RuntimeError::ToolFailed(format!("agent: handle '{handle}' not found")))
    }

    pub fn remove(&self, handle: &str) {
        self.entries.lock().unwrap().remove(handle);
    }
}

impl Tool for AgentSpawn {
    fn name(&self) -> &str {
        "flow.spawn"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Spawn a DSL flow as an independent sub-agent with its own message history and \
             iteration counter. All named args except `flow` and `async` pass through to the \
             flow as parameters — call flow.list first to discover available flows and their \
             parameter signatures.\n\n\
             Flow reference syntax: `file@flow_name`\n\
             - \"subagent.at@subagent\" — run the `subagent` flow in subagent.at\n\
             - \"subagent@research_loop\" — .at suffix optional\n\
             - \"subagent.at\" — no @, takes first non-describe flow\n\
             - \"/abs/path/my.at@main\" — absolute path\n\n\
             Default flow is `subagent.at` (research/verify/implement/review roles). \
             Required: `flow`. `async` is optional (default true). Other named args pass through to the flow. \
             Use flow.status/flow.output/flow.kill to manage async sub-agents by handle. \
             Best practice: call flow.list to see available flows and params, then pass \
             matching named args. Missing params use flow-defined defaults.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "flow": {"type": "string"},
                "async": {"type": "boolean", "default": true}
            },
            "required": ["flow"],
            "additionalProperties": true
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let is_async = args
                .named("async")
                .and_then(|v| {
                    if let Value::Bool(b) = v {
                        Some(*b)
                    } else {
                        None
                    }
                })
                .unwrap_or(true);
            if is_async {
                run_sub_agent_async(args, ctx).await
            } else {
                run_sub_agent(args, ctx).await
            }
        })
    }
}

async fn run_sub_agent(args: ToolArgs, ctx: &ToolCtx) -> ToolResult {
    let goal = match args.named("goal") {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let flow = extract_flow(&args)?.unwrap_or_else(|| "subagent.at".to_string());
    run_flow_agent(&flow, goal, &args, ctx, FlowRunId::now()).await
}

async fn run_sub_agent_async(args: ToolArgs, ctx: &ToolCtx) -> ToolResult {
    let goal = match args.named("goal") {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let model = extract_string(&args, "model", 0).unwrap_or_else(|_| "smart".to_string());
    let agent_registry = ctx.agent_registry.clone().ok_or_else(|| {
        RuntimeError::ToolFailed("flow.spawn: no agent registry available on ctx".into())
    })?;

    let flow_ref = extract_flow(&args)?.unwrap_or_else(|| "subagent.at".to_string());

    let handle = format!("agent_{}", uuid::Uuid::now_v7().simple());
    let child_run_id = FlowRunId::now();
    let entry = agent_registry.create_entry(
        handle.clone(),
        goal.clone().unwrap_or_default(),
        model,
        child_run_id.clone(),
    );

    let task_registry = ctx.task_registry.clone();
    let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
    let task_id = task_registry.as_ref().map(|tr| {
        tr.register(
            crate::task_registry::TaskKind::Agent,
            goal.clone().unwrap_or_default(),
            handle.clone(),
            session_id,
            entry.cancel.clone(),
        )
    });

    let entry_clone = Arc::clone(&entry);
    let ctx_clone = ctx.clone();
    let parent_stream_tx = ctx.stream_tx.clone();
    let child_run_id_str = child_run_id.0.to_string();
    tokio::spawn(async move {
        // Send SubAgentStarted so the TUI creates a SubAgentActivity item and
        // registers child_run_id in sub_agent_run_ids for frame routing.
        if let Some(tx) = &parent_stream_tx {
            let _ = tx.send(crate::stream::StreamFrame::SubAgentStarted {
                handle: entry_clone.handle.clone(),
                goal: entry_clone.goal.clone(),
                child_run_id: child_run_id_str.clone(),
                model: entry_clone.model.clone(),
            });
        }

        // Replace ctx.cancel with entry.cancel so flow.kill can actually cancel
        // the sub-agent's flow execution. Pass entry into ctx so the DSL runtime
        // can write output/messages/iteration synchronously during LLM calls.
        // Bind the sub-agent's own compact_lock so it can compact its segment
        // independently.
        let mut ctx_for_flow = ctx_clone;
        ctx_for_flow.cancel = entry_clone.cancel.clone();
        ctx_for_flow.agent_entry = Some(Arc::clone(&entry_clone));
        ctx_for_flow.compact_lock_handle = Some(Arc::clone(&entry_clone.compact_lock));

        let result = run_flow_agent(
            &flow_ref,
            Some(entry_clone.goal.clone()),
            &args,
            &ctx_for_flow,
            child_run_id.clone(),
        )
        .await;

        let status = match &result {
            Ok(Value::Str(s)) => AgentRunStatus::Ok {
                ended_at: chrono::Utc::now(),
                final_text: s.clone(),
            },
            Ok(_) => AgentRunStatus::Ok {
                ended_at: chrono::Utc::now(),
                final_text: String::new(),
            },
            Err(e) => AgentRunStatus::Err {
                ended_at: chrono::Utc::now(),
                message: e.to_string(),
            },
        };
        *entry_clone.status.lock().unwrap() = status.clone();

        // Send SubAgentDone so the TUI updates the SubAgentActivity item.
        if let Some(tx) = &parent_stream_tx {
            let final_text = match &status {
                AgentRunStatus::Ok { final_text, .. } => final_text.clone(),
                _ => String::new(),
            };
            let _ = tx.send(crate::stream::StreamFrame::SubAgentDone {
                handle: entry_clone.handle.clone(),
                status: status.kind_str().to_string(),
                final_text,
            });
        }

        let _ = entry_clone.stream_tx.send(AgentEvent::Exited { status });
        if let (Some(tr), Some(tid)) = (&task_registry, &task_id) {
            let ts = match result {
                Ok(_) => crate::task_registry::TaskStatus::Ok,
                Err(_) => crate::task_registry::TaskStatus::Err,
            };
            tr.finish(tid, ts);
        }
    });

    Ok(Value::Struct(vec![
        ("handle".into(), Value::Str(handle)),
        ("status".into(), Value::Str("running".into())),
    ]))
}

pub struct AgentStatus;
impl Tool for AgentStatus {
    fn name(&self) -> &str {
        "flow.status"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Check the status of an async sub-agent. Returns handle, status, goal, and timing.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"handle": {"type": "string"}},
            "required": ["handle"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let reg = ctx
                .agent_registry
                .clone()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.status: no agent registry".into()))?;
            let entry = reg.lookup(&handle)?;
            let st = entry.status.lock().unwrap().clone();
            let goal = entry.goal.clone();
            let mut fields = vec![
                ("handle".into(), Value::Str(handle)),
                ("status".into(), Value::Str(st.kind_str().into())),
                ("goal".into(), Value::Str(goal)),
            ];
            if let AgentRunStatus::Err { message, .. } = &st {
                fields.push(("error".into(), Value::Str(message.clone())));
            }
            Ok(Value::Struct(fields))
        })
    }
}

pub struct AgentOutput;
impl Tool for AgentOutput {
    fn name(&self) -> &str {
        "flow.output"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Read accumulated assistant text from an async sub-agent.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "cursor": {"type": "integer", "default": 0},
                "limit": {"type": "integer", "default": 4096}
            },
            "required": ["handle"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let cursor = args
                .named("cursor")
                .and_then(|v| {
                    if let Value::Int(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
                .max(0) as usize;
            let limit = args
                .named("limit")
                .and_then(|v| {
                    if let Value::Int(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                })
                .unwrap_or(4096)
                .max(1) as usize;
            let reg = ctx
                .agent_registry
                .clone()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.output: no agent registry".into()))?;
            let entry = reg.lookup(&handle)?;
            let output = entry.output.lock().unwrap().clone();
            let chunk = output.chars().skip(cursor).take(limit).collect::<String>();
            let next_cursor = cursor + chunk.chars().count();
            let eof = next_cursor >= output.chars().count();
            Ok(Value::Struct(vec![
                ("handle".into(), Value::Str(handle)),
                ("chunk".into(), Value::Str(chunk)),
                ("cursor".into(), Value::Int(cursor as i64)),
                ("next_cursor".into(), Value::Int(next_cursor as i64)),
                ("eof".into(), Value::Bool(eof)),
            ]))
        })
    }
}

pub struct AgentKill;
impl Tool for AgentKill {
    fn name(&self) -> &str {
        "flow.kill"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn description(&self) -> Option<&str> {
        Some("Cancel a running async sub-agent by handle.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"handle": {"type": "string"}},
            "required": ["handle"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let reg = ctx
                .agent_registry
                .clone()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.kill: no agent registry".into()))?;
            let entry = reg.lookup(&handle)?;
            entry.cancel.cancel();
            Ok(Value::Unit)
        })
    }
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

async fn run_flow_agent(
    flow_ref: &str,
    goal: Option<String>,
    args: &ToolArgs,
    ctx: &ToolCtx,
    run_id: FlowRunId,
) -> ToolResult {
    let Some(registry) = ctx.registry.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "flow.spawn: no tool registry available on ctx".into(),
        ));
    };
    let Some(providers) = ctx.providers.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "flow.spawn: no provider registry available on ctx".into(),
        ));
    };
    let (file_part, flow_name) = match flow_ref.split_once('@') {
        Some((f, n)) => (f, Some(n.to_string())),
        None => (flow_ref, None),
    };
    let (path, src) = read_flow_source(file_part).await?;
    let file = atman_dsl::parse::parse_file(&src).map_err(|e| {
        RuntimeError::ToolFailed(format!("flow.spawn: parse {}: {e}", path.display()))
    })?;
    let flow = match &flow_name {
        Some(name) => file
            .flows
            .iter()
            .find(|f| f.name.name == *name)
            .ok_or_else(|| {
                let available: Vec<&str> =
                    file.flows.iter().map(|f| f.name.name.as_str()).collect();
                RuntimeError::ToolFailed(format!(
                    "flow.spawn: flow `{name}` not found in {}. available: {}",
                    path.display(),
                    available.join(", ")
                ))
            })?,
        None => file
            .flows
            .iter()
            .find(|f| f.name.name != "describe")
            .ok_or_else(|| {
                RuntimeError::ToolFailed(format!(
                    "flow.spawn: no entry flow in {} (all flows are describe())",
                    path.display()
                ))
            })?,
    };
    let mut flow_args: Vec<(String, Value)> = Vec::new();
    if let Some(first_param) = flow.params.first()
        && let Some(g) = &goal
    {
        flow_args.push((first_param.name.name.clone(), Value::Str(g.clone())));
    }
    for (key, value) in &args.named {
        if key == "flow" || key == "async" || key == "goal" {
            continue;
        }
        if flow.params.iter().any(|p| p.name.name == *key) {
            flow_args.push((key.clone(), value.clone()));
        }
    }
    let flows = file
        .flows
        .iter()
        .map(|flow| (flow.name.name.clone(), flow.clone()))
        .collect();
    let task_id = ctx.task_registry.as_ref().map(|tr| {
        tr.register(
            crate::task_registry::TaskKind::Agent,
            flow.name.name.clone(),
            run_id.0.to_string(),
            ctx.session_id.clone().unwrap_or_else(|| "anon".into()),
            ctx.cancel.clone(),
        )
    });
    emit_flow_agent_start(ctx, &run_id, &flow.name.name);
    let mut child_ctx = sanitize_child_ctx(ctx);
    // Unify the sub-agent's message store: if an AgentEntry is bound (async
    // spawn path), point session_messages_handle at entry.messages so there is
    // ONE message segment per FlowRun. Sync
    // flow.spawn has no entry, so give it a fresh ephemeral handle.
    child_ctx.session_messages_handle = match &ctx.agent_entry {
        Some(entry) => Some(std::sync::Arc::clone(&entry.messages)),
        None => Some(std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))),
    };
    // Sync flow.spawn path has no AgentEntry; give it a fresh compact_lock so
    // session.push / compaction within the child don't deadlock on the parent's
    // lock and the child can compact its own segment. Async path gets its
    // lock from the entry (set in run_sub_agent_async).
    if ctx.agent_entry.is_none() {
        child_ctx.compact_lock_handle = Some(std::sync::Arc::new(tokio::sync::Mutex::new(())));
    }
    let out = crate::exec::exec_flow_with_siblings(
        flow,
        flow_args,
        registry.as_ref(),
        &child_ctx,
        providers.as_ref(),
        &flows,
        child_ctx.events.as_ref(),
        child_ctx.turn_id.clone(),
        Some(run_id.clone()),
        None,
        child_ctx.cancel.clone(),
        None,
        path.parent().map(|p| p.to_path_buf()),
    )
    .await;
    let status = match &out {
        Ok(_) => FlowStatus::Ok,
        Err(e) => FlowStatus::Errored {
            message: e.to_string(),
        },
    };
    emit_child_flow_end(ctx, &run_id, &status);
    if let (Some(tr), Some(tid)) = (ctx.task_registry.as_ref(), &task_id) {
        let ts = match &status {
            FlowStatus::Ok => crate::task_registry::TaskStatus::Ok,
            FlowStatus::Cancelled => crate::task_registry::TaskStatus::Killed,
            FlowStatus::Errored { .. } => crate::task_registry::TaskStatus::Err,
        };
        tr.finish(tid, ts);
    }
    out
}

fn extract_flow(args: &ToolArgs) -> Result<Option<String>, RuntimeError> {
    match args.named("flow") {
        Some(Value::Str(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(Value::Unit) | None => Ok(None),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "flow string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

async fn read_flow_source(flow_ref: &str) -> Result<(PathBuf, String), RuntimeError> {
    for path in flow_candidates(flow_ref) {
        match tokio::fs::read_to_string(&path).await {
            Ok(src) => return Ok((path, src)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RuntimeError::ToolFailed(format!(
                    "flow.spawn: read {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(RuntimeError::ToolFailed(format!(
        "flow.spawn: flow `{flow_ref}` not found"
    )))
}

fn flow_candidates(flow_ref: &str) -> Vec<PathBuf> {
    let path = PathBuf::from(flow_ref);
    if path.is_absolute() {
        return vec![path];
    }
    let file_name = if flow_ref.ends_with(".at") {
        flow_ref.to_string()
    } else {
        format!("{flow_ref}.at")
    };
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(
            PathBuf::from(home)
                .join(".config")
                .join("atman")
                .join("commands")
                .join(&file_name),
        );
    }
    out.push(PathBuf::from(file_name));
    out
}

fn emit_flow_agent_start(ctx: &ToolCtx, run_id: &FlowRunId, flow_name: &str) {
    let parent_run_id = ctx.flow_run_id.clone();
    let parent_node_id = ctx.current_node_id.clone();
    if let Some(sink) = &ctx.events {
        sink.emit(Event::FlowStart {
            run_id: run_id.clone(),
            flow_name: flow_name.into(),
            parent_run_id: parent_run_id.clone(),
            parent_node_id: parent_node_id.clone(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FlowStart {
            run_id: run_id.0.to_string(),
            flow_name: flow_name.into(),
            parent_run_id: parent_run_id.as_ref().map(|r| r.0.to_string()),
            parent_node_id,
        });
    }
}

fn emit_child_flow_end(ctx: &ToolCtx, run_id: &FlowRunId, status: &FlowStatus) {
    if let Some(sink) = &ctx.events {
        sink.emit(Event::FlowEnd {
            run_id: run_id.clone(),
            flow_name: "agent.sub".into(),
            status: status.clone(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FlowDone {
            run_id: run_id.0.to_string(),
            flow_name: "agent.sub".into(),
            ok: matches!(status, FlowStatus::Ok),
            cancelled: false,
        });
    }
}

fn sanitize_child_ctx(parent: &ToolCtx) -> ToolCtx {
    let mut c = parent.clone();
    c.session_runtime = None;
    c.session_messages_handle = None;
    c.compact_lock_handle = None;
    c.forms = None;
    c.on_memory_recent = None;
    c
}
