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
