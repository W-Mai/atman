use std::sync::Arc;

use crate::error::RuntimeError;
use crate::memory::MemoryId;
use crate::memory::confession::{Confession, ConfessionStore};
use crate::memory::todo::{Todo, TodoStatus, TodoStore};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

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

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
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
                anchors: vec![],
                created_at: chrono::Utc::now(),
            };
            let id = self.store.append(confession).await?;
            Ok(Value::Str(id.to_string()))
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
