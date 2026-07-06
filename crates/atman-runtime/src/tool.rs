use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::value::Value;

// Not `Send`: proc-macro2 spans hold `Rc<()>`. Eval / exec await inline, never spawn.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub type ToolResult = Result<Value, RuntimeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Zero,
    One,
    Two,
    Three,
    Four,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelBehavior {
    AbortSafe,
    Revertible,
    Atomic,
    Irreversible,
}

#[derive(Debug, Default, Clone)]
pub struct ToolArgs {
    pub positional: Vec<Value>,
    pub named: Vec<(String, Value)>,
}

impl ToolArgs {
    pub fn positional(&self, index: usize) -> Result<&Value, RuntimeError> {
        self.positional
            .get(index)
            .ok_or_else(|| RuntimeError::MissingArg(format!("positional[{index}]")))
    }

    pub fn named(&self, name: &str) -> Option<&Value> {
        self.named.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }
}

#[derive(Clone, Default)]
pub struct ToolCtx {
    pub cancel: CancellationToken,
    pub turn_id: Option<crate::event::TurnId>,
    pub flow_run_id: Option<crate::event::FlowRunId>,
    pub event_seq: Option<u64>,
    pub prompt_resolver: Option<std::sync::Arc<dyn crate::rendezvous::PromptResolver>>,
    pub registry: Option<std::sync::Arc<ToolRegistry>>,
    pub sandbox: Option<std::sync::Arc<dyn crate::sandbox::Sandbox>>,
    pub events: Option<crate::event::EventSink>,
    pub stdout_broadcast: Option<tokio::sync::broadcast::Sender<String>>,
    pub session_messages: Option<std::sync::Arc<Vec<crate::message::Message>>>,
}

impl ToolCtx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_anchors(
        mut self,
        turn_id: Option<crate::event::TurnId>,
        flow_run_id: Option<crate::event::FlowRunId>,
        event_seq: Option<u64>,
    ) -> Self {
        self.turn_id = turn_id;
        self.flow_run_id = flow_run_id;
        self.event_seq = event_seq;
        self
    }

    pub fn with_registry(mut self, registry: std::sync::Arc<ToolRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn with_sandbox(mut self, sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    pub fn with_events(mut self, events: crate::event::EventSink) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_stdout_broadcast(mut self, tx: tokio::sync::broadcast::Sender<String>) -> Self {
        self.stdout_broadcast = Some(tx);
        self
    }

    pub fn with_session_messages(
        mut self,
        msgs: std::sync::Arc<Vec<crate::message::Message>>,
    ) -> Self {
        self.session_messages = Some(msgs);
        self
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn tier(&self) -> Tier;
    fn cancel_behavior(&self) -> CancelBehavior {
        CancelBehavior::AbortSafe
    }
    fn description(&self) -> Option<&str> {
        None
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult>;
}

pub fn tool_spec(tool: &dyn Tool) -> ToolSpec {
    ToolSpec {
        name: tool.name().to_string(),
        description: tool.description().map(str::to_string),
        input_schema: tool.input_schema(),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
}
