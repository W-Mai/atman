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

#[derive(Clone)]
pub struct ToolCtx {
    pub cancel: CancellationToken,
}

impl ToolCtx {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
        }
    }
}

impl Default for ToolCtx {
    fn default() -> Self {
        Self::new()
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn tier(&self) -> Tier;
    fn cancel_behavior(&self) -> CancelBehavior {
        CancelBehavior::AbortSafe
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult>;
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
