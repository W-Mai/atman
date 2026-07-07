use std::path::PathBuf;
use std::sync::Arc;

use crate::error::RuntimeError;
use crate::memory::MemoryId;
use crate::memory::confession::{Confession, ConfessionStore};
use crate::memory::goal::GoalStore;
use crate::memory::spec::SpecStore;
use crate::memory::todo::{Todo, TodoStatus, TodoStore};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct MemoryGoalGet {
    pub store: Arc<GoalStore>,
}

impl Tool for MemoryGoalGet {
    fn name(&self) -> &str {
        "memory.goal.get"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Return the current session goal (persistent, auto-injected as system prefix). Empty string when unset.",
        )
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let text = self
                .store
                .get()
                .map_err(|e| RuntimeError::ToolFailed(format!("goal.get: {e}")))?;
            Ok(Value::Str(text))
        })
    }
}

pub struct MemoryGoalSet {
    pub store: Arc<GoalStore>,
}

impl Tool for MemoryGoalSet {
    fn name(&self) -> &str {
        "memory.goal.set"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Overwrite the session goal. atman injects the goal as a system-prompt \
             prefix on every llm call in this session; it does not enter message \
             history and is never compacted or evicted.",
        )
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let text = required_string(&args, "text")?;
            self.store
                .set(&text)
                .map_err(|e| RuntimeError::ToolFailed(format!("goal.set: {e}")))?;
            Ok(Value::Unit)
        })
    }
}

pub struct MemoryRecentTurns;

impl Tool for MemoryRecentTurns {
    fn name(&self) -> &str {
        "memory.recent_turns"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Return the last N Message values (user + assistant + tool_result) from the \
             current session's event log so a flow can hand the code agent a sliding \
             history window. Reads from disk; cost O(events file size).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "n": {"type": "integer", "description": "Max message count to return (default 10)"}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let n = match args.named("n").or_else(|| args.positional(0).ok()) {
                Some(Value::Int(k)) if *k >= 0 => *k as usize,
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "non-negative int".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => 10,
            };
            if n == 0 {
                return Ok(Value::List(Vec::new()));
            }
            let Some(msgs) = ctx.session_messages.as_ref() else {
                return Ok(Value::List(Vec::new()));
            };
            let start = msgs.len().saturating_sub(n);
            let out: Vec<Value> = msgs
                .iter()
                .skip(start)
                .cloned()
                .map(Value::Message)
                .collect();
            Ok(Value::List(out))
        })
    }
}

pub struct MemoryGoalClear {
    pub store: Arc<GoalStore>,
}

impl Tool for MemoryGoalClear {
    fn name(&self) -> &str {
        "memory.goal.clear"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some("Erase the session goal so future llm calls stop receiving the goal prefix.")
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            self.store
                .clear()
                .map_err(|e| RuntimeError::ToolFailed(format!("goal.clear: {e}")))?;
            Ok(Value::Unit)
        })
    }
}

pub struct MemoryTodoSet {
    pub store: Arc<TodoStore>,
}

impl Tool for MemoryTodoSet {
    fn name(&self) -> &str {
        "memory.todo.set"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let where_ = required_string(&args, "where")?;
            let why = required_string(&args, "why")?;
            let how = required_string(&args, "how")?;
            let expected_result = required_string(&args, "expected_result")?;
            let todo = Todo {
                id: MemoryId::now(),
                where_,
                why,
                how,
                expected_result,
                status: TodoStatus::Pending,
            };
            let id = self.store.add(todo).await?;
            Ok(Value::Str(id.to_string()))
        })
    }
}

pub struct MemoryTodoDone {
    pub store: Arc<TodoStore>,
}

impl Tool for MemoryTodoDone {
    fn name(&self) -> &str {
        "memory.todo.done"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let id = required_string(&args, "id")?;
            let uuid = uuid::Uuid::parse_str(&id)
                .map_err(|e| RuntimeError::ToolFailed(format!("bad todo id: {e}")))?;
            self.store
                .set_status(&MemoryId(uuid), TodoStatus::Done)
                .await?;
            Ok(Value::Unit)
        })
    }
}

pub struct MemoryConfess {
    pub store: Arc<ConfessionStore>,
}

impl Tool for MemoryConfess {
    fn name(&self) -> &str {
        "memory.confess"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Record a confession when the agent broke a rule. Anchors are auto-filled from \
             the current turn / flow_run / event_seq. Returns the new confession id.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "trigger": {"type": "string", "description": "What the user or watcher noticed."},
                "rule_violated": {"type": "string", "description": "Name of the red-line rule."},
                "what_i_did": {"type": "string", "description": "The concrete mistake."},
                "why": {"type": "string", "description": "The reasoning that led there."},
                "mitigation": {"type": "string", "description": "What will prevent recurrence."},
                "anchors": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional extra anchor strings (auto-filled ones stay)."
                }
            },
            "required": ["trigger", "rule_violated", "what_i_did", "why", "mitigation"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        let anchors = collect_anchors(&args, ctx);
        Box::pin(async move {
            let trigger = required_string(&args, "trigger")?;
            let rule_violated = required_string(&args, "rule_violated")?;
            let what_i_did = required_string(&args, "what_i_did")?;
            let why = required_string(&args, "why")?;
            let mitigation = required_string(&args, "mitigation")?;
            let confession = Confession {
                id: MemoryId::now(),
                trigger,
                rule_violated,
                what_i_did,
                why,
                mitigation,
                anchors,
                created_at: chrono::Utc::now(),
            };
            let id = self.store.append(confession).await?;
            Ok(Value::Str(id.to_string()))
        })
    }
}

fn collect_anchors(args: &ToolArgs, ctx: &ToolCtx) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(flow_run) = &ctx.flow_run_id {
        out.push(format!("flow_run:{flow_run}"));
    }
    if let Some(turn) = &ctx.turn_id {
        out.push(format!("turn:{turn}"));
    }
    if let Some(seq) = ctx.event_seq {
        out.push(format!("event_seq:{seq}"));
    }
    if let Some(Value::List(items)) = args.named("anchors") {
        for item in items {
            if let Value::Str(s) = item {
                out.push(s.clone());
            }
        }
    }
    out
}

pub struct MemorySpecStatus {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecStatus {
    fn name(&self) -> &str {
        "memory.spec.status"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let st = self.store.status(&feature).await?;
            Ok(Value::Struct(vec![
                ("feature".into(), Value::Str(st.feature)),
                ("phase".into(), Value::Str(st.phase)),
                ("entry_count".into(), Value::Int(st.entry_count as i64)),
                (
                    "deviation_count".into(),
                    Value::Int(st.deviation_count as i64),
                ),
            ]))
        })
    }
}

pub struct MemorySpecUpdate {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecUpdate {
    fn name(&self) -> &str {
        "memory.spec.update"
    }
    fn tier(&self) -> Tier {
        Tier::One
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let phase = required_string(&args, "phase")?;
            let content = required_string(&args, "content")?;
            let entry = self.store.update(&feature, &phase, content).await?;
            Ok(Value::Struct(vec![
                ("id".into(), Value::Str(entry.id.to_string())),
                ("feature".into(), Value::Str(entry.feature)),
                ("phase".into(), Value::Str(entry.phase)),
            ]))
        })
    }
}

pub struct MemorySpecDeviate {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecDeviate {
    fn name(&self) -> &str {
        "memory.spec.deviate"
    }
    fn tier(&self) -> Tier {
        Tier::One
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let section = required_string(&args, "section")?;
            let delta = required_string(&args, "delta")?;
            let reason = required_string(&args, "reason")?;
            let dev = self.store.deviate(&feature, section, delta, reason).await?;
            Ok(Value::Struct(vec![
                ("id".into(), Value::Str(dev.id.to_string())),
                ("feature".into(), Value::Str(dev.feature)),
                ("section".into(), Value::Str(dev.section)),
            ]))
        })
    }
}

pub struct MemoryFetchConfessions {
    pub store: Arc<ConfessionStore>,
}

impl Tool for MemoryFetchConfessions {
    fn name(&self) -> &str {
        "memory.fetch_confessions"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = match args.named("trigger") {
                Some(Value::Str(needle)) => self.store.find_by_trigger(needle).await?,
                _ => self.store.list().await?,
            };
            let list = items
                .into_iter()
                .map(|c| {
                    Value::Struct(vec![
                        ("id".into(), Value::Str(c.id.to_string())),
                        ("trigger".into(), Value::Str(c.trigger)),
                        ("rule_violated".into(), Value::Str(c.rule_violated)),
                        ("mitigation".into(), Value::Str(c.mitigation)),
                    ])
                })
                .collect();
            Ok(Value::List(list))
        })
    }
}

pub struct MemoryHistorySearch;

impl Tool for MemoryHistorySearch {
    fn name(&self) -> &str {
        "memory.history.search"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Full-text search the current session's chat history (or optionally every session \
             in the same project). Use it to recall past turns that fell out of your working \
             context — e.g. `plan we agreed on this morning`, `which files did we read`, \
             `error the user reported earlier`. NOT for searching source code; use fs.grep for \
             that. Params: query (FTS5 syntax, required), scope (\"session\"|\"project\", \
             default \"session\"), limit (int, default 10, max 50).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "scope": {"type": "string", "enum": ["session", "project"], "default": "session"},
                "limit": {"type": "integer", "default": 10}
            },
            "required": ["query"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let query = required_string(&args, "query")?;
            if query.trim().is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.search: empty query".into(),
                ));
            }
            let scope = match args.named("scope") {
                Some(Value::Str(s)) if s == "project" => HistoryScope::Project,
                _ => HistoryScope::Session,
            };
            let limit = match args.named("limit") {
                Some(Value::Int(n)) if *n > 0 => (*n as usize).min(50),
                _ => 10,
            };
            let Some(session_dir) = ctx.session_dir.as_ref() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.search: no session dir on context".into(),
                ));
            };
            let dirs = match scope {
                HistoryScope::Session => vec![session_dir.clone()],
                HistoryScope::Project => sibling_sessions_for_project(session_dir)
                    .unwrap_or_else(|| vec![session_dir.clone()]),
            };
            let mut hits: Vec<Value> = Vec::new();
            for dir in dirs {
                let idx = match crate::index::AnchorIndex::open_session(&dir) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                let rows = match idx.fts_search_events(&query, limit) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let sid = dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                for row in rows {
                    let snippet: String = row
                        .payload
                        .chars()
                        .take(200)
                        .collect::<String>()
                        .replace('\n', " ");
                    hits.push(Value::Struct(vec![
                        ("session_id".into(), Value::Str(sid.clone())),
                        ("seq".into(), Value::Int(row.seq as i64)),
                        ("ts".into(), Value::Str(row.ts.clone())),
                        ("kind".into(), Value::Str(row.kind.clone())),
                        ("snippet".into(), Value::Str(snippet)),
                    ]));
                }
                if hits.len() >= limit {
                    break;
                }
            }
            hits.truncate(limit);
            Ok(Value::List(hits))
        })
    }
}

pub struct MemoryHistoryRead;

impl Tool for MemoryHistoryRead {
    fn name(&self) -> &str {
        "memory.history.read"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Paginate through past messages of a session by turn index. Prefer \
             memory.history.search first to find a hit, then call this for surrounding context. \
             Params: session_id (string, default current session's directory name), offset \
             (1-based turn index, default 1), limit (int, default 20, max 100), role_filter \
             (comma-separated: user,assistant,tool; default all).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "offset": {"type": "integer", "default": 1},
                "limit": {"type": "integer", "default": 20},
                "role_filter": {"type": "string"}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let Some(current_dir) = ctx.session_dir.as_ref() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.read: no session dir on context".into(),
                ));
            };
            let dir = match args.named("session_id") {
                Some(Value::Str(sid)) if !sid.is_empty() => {
                    let sessions_parent = current_dir.parent().unwrap_or(current_dir);
                    let candidate = sessions_parent.join(sid);
                    if !candidate.is_dir() {
                        return Err(RuntimeError::ToolFailed(format!(
                            "memory.history.read: session `{sid}` not found at {}",
                            candidate.display()
                        )));
                    }
                    candidate
                }
                _ => current_dir.clone(),
            };
            let offset = match args.named("offset") {
                Some(Value::Int(n)) if *n >= 1 => *n as usize,
                _ => 1,
            };
            let limit = match args.named("limit") {
                Some(Value::Int(n)) if *n >= 1 => (*n as usize).min(100),
                _ => 20,
            };
            let role_filter: Option<Vec<String>> = match args.named("role_filter") {
                Some(Value::Str(s)) if !s.is_empty() => Some(
                    s.split(',')
                        .map(|t| t.trim().to_lowercase())
                        .filter(|t| !t.is_empty())
                        .collect(),
                ),
                _ => None,
            };
            let messages = load_session_messages(&dir, role_filter.as_deref())?;
            let total = messages.len();
            let start = offset.saturating_sub(1);
            let end = (start + limit).min(total);
            let slice: Vec<Value> = if start >= total {
                Vec::new()
            } else {
                messages[start..end]
                    .iter()
                    .cloned()
                    .map(Value::Message)
                    .collect()
            };
            let header = format!("[history: turns {start}-{end} of {total}]");
            Ok(Value::Struct(vec![
                ("header".into(), Value::Str(header)),
                ("turns".into(), Value::List(slice)),
            ]))
        })
    }
}

enum HistoryScope {
    Session,
    Project,
}

fn sibling_sessions_for_project(session_dir: &std::path::Path) -> Option<Vec<PathBuf>> {
    let meta = crate::session_meta::SessionMeta::load(session_dir)?;
    let want = meta.project_fingerprint.as_deref()?;
    let sessions_parent = session_dir.parent()?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(sessions_parent).ok()? {
        let entry = entry.ok()?;
        if !entry.path().is_dir() {
            continue;
        }
        let peer_meta = crate::session_meta::SessionMeta::load(&entry.path());
        let fp = peer_meta
            .as_ref()
            .and_then(|m| m.project_fingerprint.clone());
        if fp.as_deref() == Some(want) {
            out.push(entry.path());
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn load_session_messages(
    session_dir: &std::path::Path,
    role_filter: Option<&[String]>,
) -> Result<Vec<crate::message::Message>, RuntimeError> {
    let events_path = session_dir.join("events.jsonl");
    let contents = match std::fs::read_to_string(&events_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(RuntimeError::ToolFailed(format!(
                "memory.history.read: reading {} failed: {e}",
                events_path.display()
            )));
        }
    };
    let mut out = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if !matches!(
            kind,
            "user_msg" | "assistant_msg" | "tool_result_msg" | "system_msg"
        ) {
            continue;
        }
        let message_json = match value.get("message") {
            Some(m) => m,
            None => continue,
        };
        let msg: crate::message::Message = match serde_json::from_value(message_json.clone()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Some(filter) = role_filter {
            let role = msg.role.as_str();
            if !filter.iter().any(|f| f == role) {
                continue;
            }
        }
        out.push(msg);
    }
    Ok(out)
}

fn required_string(args: &ToolArgs, name: &str) -> Result<String, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}
