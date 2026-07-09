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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApprovalLevel {
    Auto,
    Approve,
    Dangerous,
}

impl ApprovalLevel {
    pub fn from_tier(tier: Tier) -> Self {
        match tier {
            Tier::Zero => ApprovalLevel::Auto,
            Tier::One | Tier::Two => ApprovalLevel::Approve,
            Tier::Three | Tier::Four => ApprovalLevel::Dangerous,
        }
    }
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
    pub current_node_id: Option<String>,
    pub stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    pub read_files:
        Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>>,
    pub approval: Option<std::sync::Arc<crate::session::ApprovalRegistry>>,
    pub forms: Option<std::sync::Arc<crate::session::FormRegistry>>,
    pub providers: Option<std::sync::Arc<crate::provider::ProviderRegistry>>,
    pub session_dir: Option<std::path::PathBuf>,
    pub data_root: Option<std::path::PathBuf>,
    pub project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
    pub fs_access: crate::fs_access::FsAccessPolicy,
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

    pub fn with_current_node(mut self, node_id: Option<String>) -> Self {
        self.current_node_id = node_id;
        self
    }

    pub fn with_read_files(
        mut self,
        set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    ) -> Self {
        self.read_files = Some(set);
        self
    }

    pub fn with_providers(
        mut self,
        providers: std::sync::Arc<crate::provider::ProviderRegistry>,
    ) -> Self {
        self.providers = Some(providers);
        self
    }

    pub fn with_session_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.session_dir = Some(dir);
        self
    }

    pub fn with_data_root(mut self, dir: std::path::PathBuf) -> Self {
        self.data_root = Some(dir);
        self
    }

    pub fn with_approval(
        mut self,
        approval: std::sync::Arc<crate::session::ApprovalRegistry>,
    ) -> Self {
        self.approval = Some(approval);
        self
    }

    pub fn with_fs_access(mut self, policy: crate::fs_access::FsAccessPolicy) -> Self {
        self.fs_access = policy;
        self
    }

    pub fn with_forms(mut self, forms: std::sync::Arc<crate::session::FormRegistry>) -> Self {
        self.forms = Some(forms);
        self
    }

    pub fn note_read(&self, path: &std::path::Path) {
        if let Some(set) = &self.read_files
            && let Ok(mut lock) = set.lock()
        {
            lock.insert(path.to_path_buf());
        }
    }

    pub fn has_read(&self, path: &std::path::Path) -> bool {
        self.read_files
            .as_ref()
            .and_then(|set| set.lock().ok().map(|lock| lock.contains(path)))
            .unwrap_or(false)
    }

    pub fn with_project_index(mut self, idx: std::sync::Arc<crate::index::AnchorIndex>) -> Self {
        self.project_index = Some(idx);
        self
    }

    pub fn with_stream_tx(
        mut self,
        tx: tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
    ) -> Self {
        self.stream_tx = Some(tx);
        self
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn tier(&self) -> Tier;
    fn approval_level(&self) -> ApprovalLevel {
        ApprovalLevel::from_tier(self.tier())
    }
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
    fn preview_call<'a>(
        &'a self,
        _args: &'a ToolArgs,
        _ctx: &'a ToolCtx,
    ) -> BoxFut<'a, Option<String>> {
        Box::pin(async { None })
    }
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

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<dyn Tool>)> {
        self.tools.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_level_default_maps_from_tier() {
        assert_eq!(ApprovalLevel::from_tier(Tier::Zero), ApprovalLevel::Auto);
        assert_eq!(ApprovalLevel::from_tier(Tier::One), ApprovalLevel::Approve);
        assert_eq!(ApprovalLevel::from_tier(Tier::Two), ApprovalLevel::Approve);
        assert_eq!(
            ApprovalLevel::from_tier(Tier::Three),
            ApprovalLevel::Dangerous
        );
        assert_eq!(
            ApprovalLevel::from_tier(Tier::Four),
            ApprovalLevel::Dangerous
        );
    }

    #[test]
    fn approval_level_ordered_auto_lt_approve_lt_dangerous() {
        assert!(ApprovalLevel::Auto < ApprovalLevel::Approve);
        assert!(ApprovalLevel::Approve < ApprovalLevel::Dangerous);
    }
}
