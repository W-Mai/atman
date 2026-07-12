use std::sync::Arc;

use crate::error::RuntimeError;
use crate::memory::plan::{Plan, PlanStore};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct PlanWrite {
    pub store: Arc<PlanStore>,
}

impl Tool for PlanWrite {
    fn name(&self) -> &str {
        "plan.write"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Create or overwrite a step-by-step plan. atman injects the plan as a \
             system-prompt prefix on every LLM call so the agent always knows what \
             step it's on. Use plan.tick to mark steps complete.\n\n\
             Best practice: call plan.write early, right after setting the goal. \
             Break work into 3-8 concrete steps. Each step should be a single \
             actionable verb: 'read auth.rs', 'add validate_token function', \
             'write regression test for empty token'. Update the plan if you \
             discover the approach needs to change. Use plan.tick after each step.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "title": {"type": "string"},
                "steps": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["title", "steps"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let title = required_string(&args, "title")?;
            let steps = required_string_list(&args, "steps")?;
            let id = match args.named("id") {
                Some(Value::Str(s)) if !s.is_empty() => s.clone(),
                _ => slug_from_title(&title),
            };
            let plan = Plan::new(id.clone(), title, steps);
            self.store
                .upsert(plan)
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("plan.write: {e}")))?;
            Ok(Value::Str(id))
        })
    }
}

pub struct PlanRead {
    pub store: Arc<PlanStore>,
}

impl Tool for PlanRead {
    fn name(&self) -> &str {
        "plan.read"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Read the current plan as a markdown checklist with progress markers. \
             Without `id`, returns the most recently updated plan. \
             Returns empty string if no plan exists. Call this to refresh your \
             memory of the plan before starting a new step.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let plan = match args.named("id") {
                Some(Value::Str(s)) if !s.is_empty() => self
                    .store
                    .get(s)
                    .await
                    .map_err(|e| RuntimeError::ToolFailed(format!("plan.read: {e}")))?,
                _ => self
                    .store
                    .latest()
                    .await
                    .map_err(|e| RuntimeError::ToolFailed(format!("plan.read: {e}")))?,
            };
            let Some(plan) = plan else {
                return Ok(Value::Str(String::new()));
            };
            Ok(Value::Str(render_plan(&plan)))
        })
    }
}

pub struct PlanTick {
    pub store: Arc<PlanStore>,
}

impl Tool for PlanTick {
    fn name(&self) -> &str {
        "plan.tick"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Mark a plan step as done (0-based index). Without `id`, targets the \
             latest plan. Returns the updated plan as markdown. Call this right \
             after completing each step to keep progress visible in the sidebar.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "step_index": {"type": "integer"}
            },
            "required": ["step_index"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let step_index = required_usize(&args, "step_index")?;
            let plan_id = match args.named("id") {
                Some(Value::Str(s)) if !s.is_empty() => s.clone(),
                _ => match self
                    .store
                    .latest()
                    .await
                    .map_err(|e| RuntimeError::ToolFailed(format!("plan.tick: {e}")))?
                {
                    Some(p) => p.id,
                    None => {
                        return Err(RuntimeError::ToolFailed(
                            "plan.tick: no plan exists yet — call plan.write first".into(),
                        ));
                    }
                },
            };
            self.store
                .tick(&plan_id, step_index)
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("plan.tick: {e}")))?;
            let plan = self
                .store
                .get(&plan_id)
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("plan.tick: {e}")))?
                .ok_or_else(|| {
                    RuntimeError::ToolFailed(format!("plan.tick: plan `{plan_id}` disappeared"))
                })?;
            Ok(Value::Str(render_plan(&plan)))
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

fn required_string_list(args: &ToolArgs, name: &str) -> Result<Vec<String>, RuntimeError> {
    match args.named(name) {
        Some(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Str(s) => out.push(s.clone()),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "list of strings".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                }
            }
            Ok(out)
        }
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "list of strings".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn required_usize(args: &ToolArgs, name: &str) -> Result<usize, RuntimeError> {
    match args.named(name) {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as usize),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "non-negative int".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

pub fn render_plan(plan: &Plan) -> String {
    let (done, total) = plan.progress();
    let mut out = format!(
        "# Plan: {}\n_id: {} · {}/{} done_\n\n",
        plan.title, plan.id, done, total
    );
    for step in &plan.steps {
        let mark = if step.done { "[x]" } else { "[ ]" };
        out.push_str(&format!("- {mark} {}\n", step.text));
    }
    out
}

fn slug_from_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len().min(48));
    let mut last_dash = false;
    for c in title.chars().take(48) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        format!("plan-{}", uuid::Uuid::now_v7().simple())
    } else {
        out
    }
}
