use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::value::Value;

/// Sendable boxed future used by provider and tool traits.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type ToolResult = Result<Value, RuntimeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationPlane {
    Ordinary,
    PermissionControl,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistorySegment {
    #[default]
    Root,
    Spawned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathOrigin {
    Omitted,
    Relative,
    ExplicitInside,
    ExplicitExternal,
    Unbound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: std::path::PathBuf,
    pub origin: PathOrigin,
}

#[derive(Clone, Default)]
pub struct ToolCtx {
    pub cancel: CancellationToken,
    pub flow_cancel: CancellationToken,
    pub turn_id: Option<crate::event::TurnId>,
    pub flow_run_id: Option<crate::event::FlowRunId>,
    pub history_segment: HistorySegment,
    pub event_seq: Option<u64>,
    pub prompt_resolver: Option<std::sync::Arc<dyn crate::rendezvous::PromptResolver>>,
    pub registry: Option<std::sync::Arc<ToolRegistry>>,
    pub sandbox: Option<std::sync::Arc<dyn crate::sandbox::Sandbox>>,
    pub events: Option<crate::event::EventSink>,
    pub stdout_broadcast: Option<tokio::sync::broadcast::Sender<String>>,
    pub session_messages: Option<std::sync::Arc<Vec<crate::message::Message>>>,
    pub(crate) context_owner: Option<ContextOwner>,
    pub current_node_id: Option<String>,
    pub stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    pub read_files:
        Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>>,
    pub approval: Option<std::sync::Arc<crate::session::ApprovalRegistry>>,
    pub permission_broker: Option<std::sync::Arc<crate::permission::PermissionBroker>>,
    pub(crate) invocation_authorization: Option<crate::permission::InvocationAuthorization>,
    pub forms: Option<std::sync::Arc<crate::session::FormRegistry>>,
    pub providers: Option<std::sync::Arc<crate::provider::ProviderRegistry>>,
    pub session_dir: Option<std::path::PathBuf>,
    pub output_store: Option<std::sync::Arc<crate::tools::tool_output::OutputStore>>,
    pub data_root: Option<std::path::PathBuf>,
    pub project_index: Option<std::sync::Arc<crate::index::AnchorIndex>>,
    pub fs_access: crate::fs_access::FsAccessPolicy,
    pub workspace: Option<crate::git_workspace::WorkspaceBinding>,
    pub flow_workspace_service: Option<std::sync::Arc<crate::flow_workspace::FlowWorkspaceService>>,
    pub lifecycle_fire_tx:
        Option<tokio::sync::mpsc::UnboundedSender<atman_dsl::ast::LifecycleEvent>>,
    pub bg_registry: Option<std::sync::Arc<crate::tools::bash_bg::BgRegistry>>,
    pub term_registry: Option<std::sync::Arc<crate::tools::term::TermRegistry>>,
    pub watch_hub: Option<std::sync::Arc<crate::watch::WatchHub>>,
    pub flow_registry: Option<std::sync::Arc<crate::tools::agent_ctrl::FlowRegistry>>,
    pub flow_identity: Option<std::sync::Arc<crate::flow_authority::FlowIdentity>>,
    pub task_registry: Option<crate::task_registry::TaskRegistry>,
    pub session_id: Option<String>,
    pub trust: Option<crate::trust::TrustConfig>,
    pub safety: Option<crate::safety::SafetyConfig>,
    pub current_model: Option<String>,
    pub call_intent: Option<crate::message::ToolCallIntent>,
    pub tool_use_id: Option<String>,
    pub(crate) model_tool_exposures: Option<ToolExposureRegistry>,
    pub(crate) invocation_env: crate::invocation_env::InvocationEnv,
    pub watch_rules: Option<crate::streaming::WatchRules>,
    /// Called when memory.recent_turns is invoked, with the count of returned messages.
    pub on_memory_recent: Option<std::sync::Arc<dyn Fn(u16) + Send + Sync>>,
    pub history_store: Option<std::sync::Arc<dyn crate::history_store::HistoryStore>>,
    pub agent_entry: Option<std::sync::Arc<crate::tools::agent_ctrl::FlowEntry>>,
    pub tool_output_budget: crate::tools::tool_output::ToolOutputBudget,
}

#[derive(Clone)]
pub(crate) enum ContextOwner {
    Session(std::sync::Arc<crate::session::Session>),
    Detached(std::sync::Arc<crate::context_state::ContextState>),
}

#[derive(Clone, Default)]
pub(crate) struct ToolExposureRegistry {
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<ToolExposureKey, usize>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ToolExposureKey {
    flow_run_id: Option<crate::event::FlowRunId>,
    tool_use_id: String,
    tool_name: String,
}

impl ToolExposureRegistry {
    pub(crate) fn register_response<I, S>(
        &self,
        flow_run_id: Option<&crate::event::FlowRunId>,
        message: &crate::message::Message,
        exposed_names: I,
    ) where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let exposed_names: std::collections::HashSet<String> = exposed_names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect();
        let mut id_counts = std::collections::HashMap::new();
        for part in &message.parts {
            if let crate::message::MessagePart::ToolUse { id, .. } = part {
                *id_counts.entry(id.as_str()).or_insert(0usize) += 1;
            }
        }

        let mut pending = self.pending.lock().unwrap();
        for part in &message.parts {
            let crate::message::MessagePart::ToolUse { id, name, .. } = part else {
                continue;
            };
            if id_counts.get(id.as_str()) != Some(&1) || !exposed_names.contains(name) {
                continue;
            }
            *pending
                .entry(ToolExposureKey {
                    flow_run_id: flow_run_id.cloned(),
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                })
                .or_insert(0) += 1;
        }
    }

    pub(crate) fn claim(
        &self,
        flow_run_id: Option<&crate::event::FlowRunId>,
        tool_use_id: &str,
        tool_name: &str,
    ) -> bool {
        let key = ToolExposureKey {
            flow_run_id: flow_run_id.cloned(),
            tool_use_id: tool_use_id.to_string(),
            tool_name: tool_name.to_string(),
        };
        let mut pending = self.pending.lock().unwrap();
        let Some(count) = pending.get_mut(&key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            pending.remove(&key);
        }
        true
    }
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

    pub fn with_history_segment(mut self, segment: HistorySegment) -> Self {
        self.history_segment = segment;
        self
    }

    pub fn with_call_intent(mut self, call_intent: Option<crate::message::ToolCallIntent>) -> Self {
        self.call_intent = call_intent;
        self
    }

    pub fn with_tool_use_id(mut self, tool_use_id: impl Into<String>) -> Self {
        self.tool_use_id = Some(tool_use_id.into());
        self
    }

    pub(crate) fn with_invocation_env(
        mut self,
        invocation_env: crate::invocation_env::InvocationEnv,
    ) -> Self {
        self.invocation_env = invocation_env;
        self
    }

    pub fn message_flow_run_id(&self) -> Option<crate::event::FlowRunId> {
        match self.history_segment {
            HistorySegment::Root => None,
            HistorySegment::Spawned => self.flow_run_id.clone(),
        }
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

    pub fn with_context(
        mut self,
        context: std::sync::Arc<crate::context_state::ContextState>,
    ) -> Self {
        self.context_owner = Some(ContextOwner::Detached(context));
        self
    }

    pub fn with_session_runtime(
        mut self,
        session: std::sync::Arc<crate::session::Session>,
    ) -> Self {
        self.context_owner = Some(ContextOwner::Session(session));
        self
    }

    pub(crate) fn clear_context(&mut self) {
        self.context_owner = None;
    }

    pub fn session_runtime(&self) -> Option<&std::sync::Arc<crate::session::Session>> {
        match self.context_owner.as_ref()? {
            ContextOwner::Session(session) => Some(session),
            ContextOwner::Detached(_) => None,
        }
    }

    pub fn context(&self) -> Option<&std::sync::Arc<crate::context_state::ContextState>> {
        Some(match self.context_owner.as_ref()? {
            ContextOwner::Session(session) => session.context(),
            ContextOwner::Detached(context) => context,
        })
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
        self.output_store = Some(std::sync::Arc::new(
            crate::tools::tool_output::OutputStore::at(dir.clone()),
        ));
        self.session_dir = Some(dir);
        self
    }

    pub fn with_output_store(
        mut self,
        store: std::sync::Arc<crate::tools::tool_output::OutputStore>,
    ) -> Self {
        self.output_store = Some(store);
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

    pub fn with_permission_broker(
        mut self,
        broker: std::sync::Arc<crate::permission::PermissionBroker>,
    ) -> Self {
        self.permission_broker = Some(broker);
        self
    }

    // A per-call clone prevents concurrent dispatch entries from sharing permits.
    pub(crate) fn authorized_for(
        &self,
        authorization: crate::permission::InvocationAuthorization,
    ) -> Self {
        let mut ctx = self.clone();
        ctx.invocation_authorization = Some(authorization);
        ctx
    }

    pub(crate) fn invocation_authorization(
        &self,
    ) -> Option<&crate::permission::InvocationAuthorization> {
        self.invocation_authorization.as_ref()
    }

    pub(crate) fn invocation_authorization_for(
        &self,
        tool_name: &str,
    ) -> Result<&crate::permission::InvocationAuthorization, crate::error::RuntimeError> {
        let authorization = self.invocation_authorization.as_ref().ok_or_else(|| {
            crate::error::RuntimeError::ToolFailed(format!(
                "{tool_name}: missing invocation authorization"
            ))
        })?;
        if authorization.tool_name() != tool_name {
            return Err(crate::error::RuntimeError::ToolFailed(format!(
                "{tool_name}: invocation authorization belongs to {}",
                authorization.tool_name()
            )));
        }
        Ok(authorization)
    }

    pub fn with_fs_access(mut self, policy: crate::fs_access::FsAccessPolicy) -> Self {
        self.fs_access = policy;
        self
    }

    pub fn with_workspace(mut self, binding: crate::git_workspace::WorkspaceBinding) -> Self {
        self.fs_access.workspace = Some(binding.path.clone());
        self.workspace = Some(binding);
        self
    }

    pub fn with_flow_workspace_service(
        mut self,
        service: std::sync::Arc<crate::flow_workspace::FlowWorkspaceService>,
    ) -> Self {
        self.flow_workspace_service = Some(service);
        self
    }

    pub fn resolve_cwd(
        &self,
        explicit: Option<&std::path::Path>,
    ) -> Result<std::path::PathBuf, RuntimeError> {
        Ok(self.resolve_cwd_with_origin(explicit)?.path)
    }

    pub fn resolve_cwd_with_origin(
        &self,
        explicit: Option<&std::path::Path>,
    ) -> Result<ResolvedPath, RuntimeError> {
        match explicit {
            Some(path) => self.resolve_path_with_origin(path),
            None => {
                let mut resolved = self.resolve_path_with_origin(std::path::Path::new("."))?;
                resolved.origin = if self.workspace.is_some() {
                    PathOrigin::Omitted
                } else {
                    PathOrigin::Unbound
                };
                Ok(resolved)
            }
        }
    }

    pub fn resolve_path(&self, path: &std::path::Path) -> Result<std::path::PathBuf, RuntimeError> {
        Ok(self.resolve_path_with_origin(path)?.path)
    }

    pub fn resolve_path_with_origin(
        &self,
        path: &std::path::Path,
    ) -> Result<ResolvedPath, RuntimeError> {
        let Some(binding) = &self.workspace else {
            let base = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let candidate = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            };
            return Ok(ResolvedPath {
                path: crate::fs_access::canonicalize_stable(&candidate),
                origin: PathOrigin::Unbound,
            });
        };
        let root = crate::fs_access::canonicalize_stable(&binding.path);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            binding.path.join(path)
        };
        let resolved = crate::fs_access::canonicalize_stable(&candidate);
        if path.is_absolute() {
            return Ok(ResolvedPath {
                origin: if resolved.starts_with(&root) {
                    PathOrigin::ExplicitInside
                } else {
                    PathOrigin::ExplicitExternal
                },
                path: resolved,
            });
        }
        if !resolved.starts_with(&root) {
            return Err(RuntimeError::ToolFailed(format!(
                "managed workspace path {} escapes workspace root {}",
                path.display(),
                binding.path.display()
            )));
        }
        Ok(ResolvedPath {
            path: resolved,
            origin: PathOrigin::Relative,
        })
    }

    pub fn with_lifecycle_fire_tx(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<atman_dsl::ast::LifecycleEvent>,
    ) -> Self {
        self.lifecycle_fire_tx = Some(tx);
        self
    }

    pub fn with_forms(mut self, forms: std::sync::Arc<crate::session::FormRegistry>) -> Self {
        self.forms = Some(forms);
        self
    }

    pub fn with_bg_registry(
        mut self,
        registry: std::sync::Arc<crate::tools::bash_bg::BgRegistry>,
    ) -> Self {
        self.bg_registry = Some(registry);
        self
    }

    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    pub fn with_trust(mut self, trust: crate::trust::TrustConfig) -> Self {
        self.trust = Some(trust);
        self
    }

    /// Freezes the active trust policy for one tool invocation. The broker uses
    /// this snapshot to mint an invocation authorization carrying the selected
    /// execution boundary.
    pub fn for_tool_invocation(mut self, _tier: Tier) -> Self {
        if let Some(session) = self.session_runtime() {
            self.trust = Some(session.trust_config());
        }
        self
    }

    pub fn with_safety(mut self, safety: crate::safety::SafetyConfig) -> Self {
        self.safety = Some(safety);
        self
    }

    pub fn with_current_model(mut self, model: impl Into<String>) -> Self {
        self.current_model = Some(model.into());
        self
    }

    pub fn with_watch_rules(mut self, rules: crate::streaming::WatchRules) -> Self {
        self.watch_rules = Some(rules);
        self
    }

    pub fn with_term_registry(
        mut self,
        registry: std::sync::Arc<crate::tools::term::TermRegistry>,
    ) -> Self {
        self.term_registry = Some(registry);
        self
    }

    pub fn with_watch_hub(mut self, hub: std::sync::Arc<crate::watch::WatchHub>) -> Self {
        self.watch_hub = Some(hub);
        self
    }

    pub fn with_flow_registry(
        mut self,
        registry: std::sync::Arc<crate::tools::agent_ctrl::FlowRegistry>,
    ) -> Self {
        self.flow_registry = Some(registry);
        self
    }

    pub fn with_task_registry(mut self, registry: crate::task_registry::TaskRegistry) -> Self {
        self.task_registry = Some(registry);
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

    pub fn with_history_store(
        mut self,
        store: std::sync::Arc<dyn crate::history_store::HistoryStore>,
    ) -> Self {
        self.history_store = Some(store);
        self
    }

    pub fn with_agent_entry(
        mut self,
        entry: std::sync::Arc<crate::tools::agent_ctrl::FlowEntry>,
    ) -> Self {
        self.agent_entry = Some(entry);
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
    fn invocation_plane(&self) -> InvocationPlane {
        InvocationPlane::Ordinary
    }
    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
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
    // Defaulting to none avoids treating arbitrary command or URL arguments as paths.
    fn invocation_provenance(
        &self,
        _args: &ToolArgs,
        _ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        Ok(crate::permission::ResourceProvenance::none())
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
    let mut input_schema = tool.input_schema();
    decorate_tool_input_schema(&mut input_schema);
    canonicalize_json_object_keys(&mut input_schema);
    ToolSpec {
        name: tool.name().to_string(),
        description: tool.description().map(str::to_string),
        input_schema,
    }
}

fn canonicalize_json_object_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
            for (_, value) in &mut entries {
                canonicalize_json_object_keys(value);
            }
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            object.extend(entries);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                canonicalize_json_object_keys(item);
            }
        }
        _ => {}
    }
}

fn tool_call_intent_schema() -> serde_json::Value {
    serde_json::json!({"type": "string"})
}

fn decorate_tool_input_schema(schema: &mut serde_json::Value) {
    let Some(root) = schema.as_object_mut() else {
        return;
    };
    if root.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return;
    }
    let properties = root
        .entry("properties")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(properties) = properties.as_object_mut() else {
        return;
    };
    if properties.contains_key(crate::message::TOOL_CALL_INTENT_FIELD) {
        return;
    }
    properties.insert(
        crate::message::TOOL_CALL_INTENT_FIELD.into(),
        tool_call_intent_schema(),
    );
}

fn tool_spec_call_intent_support(tool_name: &str, tools: &[ToolSpec]) -> Option<bool> {
    tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .map(|tool| {
            tool.input_schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .and_then(|properties| properties.get(crate::message::TOOL_CALL_INTENT_FIELD))
                == Some(&tool_call_intent_schema())
        })
}

pub fn tool_spec_supports_call_intent(tool_name: &str, tools: &[ToolSpec]) -> bool {
    tool_spec_call_intent_support(tool_name, tools) == Some(true)
}

pub fn tool_spec_blocks_call_intent(tool_name: &str, tools: &[ToolSpec]) -> bool {
    tool_spec_call_intent_support(tool_name, tools) == Some(false)
}

pub fn tool_schema_uses_call_intent_field(schema: &serde_json::Value) -> bool {
    schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|properties| properties.contains_key(crate::message::TOOL_CALL_INTENT_FIELD))
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
    tools: std::sync::Arc<std::sync::RwLock<HashMap<String, std::sync::Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, tool: std::sync::Arc<dyn Tool>) {
        assert!(
            !crate::eval::is_evaluator_intrinsic(tool.name()),
            "tool name `{}` is reserved for an evaluator intrinsic",
            tool.name()
        );
        self.tools
            .write()
            .unwrap()
            .insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<std::sync::Arc<dyn Tool>> {
        self.tools.read().unwrap().get(name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.read().unwrap().contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.read().unwrap().keys().cloned().collect()
    }

    pub fn iter(&self) -> Vec<(String, std::sync::Arc<dyn Tool>)> {
        self.tools
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Replace one qualified tool namespace while holding a single write lock.
    pub fn replace_namespace(&self, prefix: &str, tools: Vec<std::sync::Arc<dyn Tool>>) {
        assert!(!prefix.is_empty(), "tool namespace prefix cannot be empty");
        assert!(
            tools.iter().all(|tool| tool.name().starts_with(prefix)),
            "replacement tools must belong to namespace `{prefix}`"
        );
        let mut registry = self.tools.write().unwrap();
        registry.retain(|name, _| !name.starts_with(prefix));
        for tool in tools {
            registry.insert(tool.name().to_string(), tool);
        }
    }

    /// Remove namespaced tools that are no longer backed by an enabled source.
    pub fn retain_namespaces(&self, root_prefix: &str, retained_prefixes: &[String]) {
        assert!(
            retained_prefixes
                .iter()
                .all(|prefix| prefix.starts_with(root_prefix)),
            "retained namespaces must belong to root `{root_prefix}`"
        );
        self.tools.write().unwrap().retain(|name, _| {
            !name.starts_with(root_prefix)
                || retained_prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        });
    }

    /// Remove all tools whose name starts with `prefix` (e.g. `"mcp."`).
    pub fn unregister_prefix(&self, prefix: &str) {
        self.tools
            .write()
            .unwrap()
            .retain(|k, _| !k.starts_with(prefix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NamedTool(&'static str);

    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { Ok(crate::value::Value::Unit) })
        }
    }

    #[test]
    fn namespace_replacement_removes_stale_tools_without_touching_peers() {
        let registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(NamedTool("mcp.alpha.old")));
        registry.register(std::sync::Arc::new(NamedTool("mcp.beta.keep")));

        registry.replace_namespace(
            "mcp.alpha.",
            vec![std::sync::Arc::new(NamedTool("mcp.alpha.new"))],
        );

        assert!(!registry.has("mcp.alpha.old"));
        assert!(registry.has("mcp.alpha.new"));
        assert!(registry.has("mcp.beta.keep"));
    }

    #[test]
    fn namespace_retention_only_removes_disabled_sources() {
        let registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(NamedTool("mcp.alpha.keep")));
        registry.register(std::sync::Arc::new(NamedTool("mcp.beta.remove")));
        registry.register(std::sync::Arc::new(NamedTool("fs.read")));

        registry.retain_namespaces("mcp.", &["mcp.alpha.".to_string()]);

        assert!(registry.has("mcp.alpha.keep"));
        assert!(!registry.has("mcp.beta.remove"));
        assert!(registry.has("fs.read"));
    }

    #[test]
    fn model_tool_exposure_is_scoped_exact_and_single_use() {
        let exposures = ToolExposureRegistry::default();
        let run = crate::event::FlowRunId::now();
        let other_run = crate::event::FlowRunId::now();
        let response = crate::message::Message {
            role: crate::message::MessageRole::Assistant,
            parts: vec![crate::message::MessagePart::ToolUse {
                id: "call-1".into(),
                name: "allowed.probe".into(),
                input: serde_json::json!({}),
                intent: None,
            }],
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        };
        exposures.register_response(Some(&run), &response, ["allowed.probe"]);

        assert!(!exposures.claim(Some(&other_run), "call-1", "allowed.probe"));
        assert!(!exposures.claim(Some(&run), "call-1", "changed.probe"));
        assert!(exposures.claim(Some(&run), "call-1", "allowed.probe"));
        assert!(!exposures.claim(Some(&run), "call-1", "allowed.probe"));
    }

    #[test]
    fn model_tool_exposure_rejects_unexposed_and_duplicate_response_ids() {
        let exposures = ToolExposureRegistry::default();
        let run = crate::event::FlowRunId::now();
        let response = crate::message::Message {
            role: crate::message::MessageRole::Assistant,
            parts: vec![
                crate::message::MessagePart::ToolUse {
                    id: "duplicate".into(),
                    name: "allowed.probe".into(),
                    input: serde_json::json!({}),
                    intent: None,
                },
                crate::message::MessagePart::ToolUse {
                    id: "duplicate".into(),
                    name: "allowed.probe".into(),
                    input: serde_json::json!({}),
                    intent: None,
                },
                crate::message::MessagePart::ToolUse {
                    id: "hidden".into(),
                    name: "hidden.probe".into(),
                    input: serde_json::json!({}),
                    intent: None,
                },
            ],
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        };
        exposures.register_response(Some(&run), &response, ["allowed.probe"]);

        assert!(!exposures.claim(Some(&run), "duplicate", "allowed.probe"));
        assert!(!exposures.claim(Some(&run), "hidden", "hidden.probe"));
    }

    struct ReservedEnvTool;

    struct ObjectTool;

    impl Tool for ObjectTool {
        fn name(&self) -> &str {
            "probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "integer"}},
                "required": ["value"]
            })
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { Ok(Value::Unit) })
        }
    }

    struct CollidingTool;

    impl Tool for CollidingTool {
        fn name(&self) -> &str {
            "collision"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {"_atman_intent": {"type": "integer"}}
            })
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { Ok(Value::Unit) })
        }
    }

    impl Tool for ReservedEnvTool {
        fn name(&self) -> &str {
            "env"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { Ok(Value::Unit) })
        }
    }

    #[test]
    #[should_panic(expected = "tool name `env` is reserved for an evaluator intrinsic")]
    fn evaluator_intrinsic_names_cannot_be_registered_as_tools() {
        ToolRegistry::new().register(std::sync::Arc::new(ReservedEnvTool));
    }

    #[test]
    fn tool_spec_adds_optional_call_intent_without_mutating_required() {
        let spec = tool_spec(&ObjectTool);
        assert_eq!(spec.input_schema["required"], serde_json::json!(["value"]));
        assert_eq!(
            spec.input_schema["properties"][crate::message::TOOL_CALL_INTENT_FIELD],
            tool_call_intent_schema()
        );
        assert!(tool_spec_supports_call_intent("probe", &[spec]));
        assert_eq!(
            serde_json::to_vec(&tool_spec(&ObjectTool)).unwrap(),
            serde_json::to_vec(&tool_spec(&ObjectTool)).unwrap()
        );
    }

    #[test]
    fn tool_spec_preserves_business_field_collision() {
        let spec = tool_spec(&CollidingTool);
        assert_eq!(
            spec.input_schema["properties"][crate::message::TOOL_CALL_INTENT_FIELD],
            serde_json::json!({"type": "integer"})
        );
        assert!(!tool_spec_supports_call_intent("collision", &[spec]));
    }

    #[test]
    fn canonical_json_orders_nested_object_keys() {
        let mut left: serde_json::Value =
            serde_json::from_str(r#"{"zeta":{"y":1,"x":2},"alpha":0}"#).unwrap();
        let mut right: serde_json::Value =
            serde_json::from_str(r#"{"alpha":0,"zeta":{"x":2,"y":1}}"#).unwrap();
        canonicalize_json_object_keys(&mut left);
        canonicalize_json_object_keys(&mut right);
        assert_eq!(
            serde_json::to_vec(&left).unwrap(),
            serde_json::to_vec(&right).unwrap()
        );
    }

    #[test]
    fn tool_call_input_codec_round_trips_metadata_and_preserves_collisions() {
        let spec = tool_spec(&ObjectTool);
        let wire = serde_json::json!({
            "value": 7,
            "_atman_intent": "  inspect\nstate  "
        });
        let (clean, intent) =
            crate::message::decode_tool_call_input(wire, "probe", std::slice::from_ref(&spec));
        assert_eq!(clean, serde_json::json!({"value": 7}));
        assert_eq!(
            intent.as_ref().map(|value| value.as_str()),
            Some("inspect state")
        );
        assert_eq!(
            crate::message::encode_tool_call_input(&clean, intent.as_ref(), "probe", &[spec]),
            serde_json::json!({"value": 7, "_atman_intent": "inspect state"})
        );
        assert_eq!(
            crate::message::encode_tool_call_input(&clean, intent.as_ref(), "probe", &[]),
            serde_json::json!({"value": 7, "_atman_intent": "inspect state"})
        );

        let spec = tool_spec(&ObjectTool);
        let (clean, intent) = crate::message::decode_tool_call_input(
            serde_json::json!({"value": 7, "_atman_intent": 42}),
            "probe",
            &[spec],
        );
        assert_eq!(clean, serde_json::json!({"value": 7}));
        assert!(intent.is_none());

        let collision = tool_spec(&CollidingTool);
        let business_input = serde_json::json!({"_atman_intent": 42});
        let (clean, intent) = crate::message::decode_tool_call_input(
            business_input.clone(),
            "collision",
            &[collision],
        );
        assert_eq!(clean, business_input);
        assert!(intent.is_none());
    }

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

    #[test]
    fn invocation_snapshot_tracks_session_trust() {
        let dir = tempfile::tempdir().unwrap();
        let initial = crate::trust::TrustConfig::default();
        let session = std::sync::Arc::new(
            crate::session::Session::open_with_trust(dir.path(), initial.clone()).unwrap(),
        );
        let sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
            std::sync::Arc::new(crate::sandbox::SandboxExec::new(dir.path()));
        let flows = std::sync::Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
        let identity = flows
            .register_root(
                session.id().to_string(),
                crate::event::FlowRunId::now(),
                crate::flow_authority::EffectiveAuthority::root(&initial, true, None),
            )
            .unwrap();
        let mut base = ToolCtx::new()
            .with_trust(crate::trust::TrustConfig {
                mode: crate::trust::TrustMode::Reckless,
                ..crate::trust::TrustConfig::default()
            })
            .with_session_runtime(std::sync::Arc::clone(&session))
            .with_sandbox(sandbox);
        base.flow_identity = Some(identity);

        let controlled = base.clone().for_tool_invocation(Tier::Four);
        assert_eq!(controlled.trust, Some(initial));
        assert!(controlled.sandbox.is_some());

        let reckless = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Reckless,
            ..crate::trust::TrustConfig::default()
        };
        session.update_trust(reckless.clone(), |_| Ok(())).unwrap();
        let unrestricted = base.for_tool_invocation(Tier::Four);

        assert_eq!(unrestricted.trust, Some(reckless));
        assert!(unrestricted.sandbox.is_some());
        assert!(controlled.sandbox.is_some());
    }

    #[test]
    fn invocation_context_retains_sandbox_across_control_tools() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
            std::sync::Arc::new(crate::sandbox::SandboxExec::new(dir.path()));
        let base = ToolCtx::new().with_sandbox(sandbox);

        let control_ctx = base.for_tool_invocation(Tier::Two);
        assert!(control_ctx.sandbox.is_some());
        assert!(
            control_ctx
                .for_tool_invocation(Tier::Four)
                .sandbox
                .is_some()
        );
    }

    fn binding(path: std::path::PathBuf) -> crate::git_workspace::WorkspaceBinding {
        crate::git_workspace::WorkspaceBinding {
            workspace_id: "workspace".into(),
            repository_root: path.clone(),
            path,
            branch: None,
        }
    }

    #[test]
    fn workspace_resolver_rebinds_policy_without_changing_process_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        let process_cwd = std::env::current_dir().unwrap();
        let ctx = ToolCtx::new()
            .with_fs_access(crate::fs_access::FsAccessPolicy {
                mode: crate::fs_access::FsAccessMode::ReadOnly,
                workspace: Some(process_cwd.clone()),
            })
            .with_workspace(binding(workspace.path().to_path_buf()));

        let canonical_workspace = crate::fs_access::canonicalize_stable(workspace.path());
        let omitted = ctx.resolve_cwd_with_origin(None).unwrap();
        assert_eq!(omitted.path, canonical_workspace);
        assert_eq!(omitted.origin, PathOrigin::Omitted);

        let relative = ctx
            .resolve_path_with_origin(std::path::Path::new("nested/file"))
            .unwrap();
        assert_eq!(relative.path, canonical_workspace.join("nested/file"));
        assert_eq!(relative.origin, PathOrigin::Relative);

        let inside = ctx
            .resolve_path_with_origin(&workspace.path().join("inside"))
            .unwrap();
        assert_eq!(inside.origin, PathOrigin::ExplicitInside);

        let external = ctx.resolve_path_with_origin(&process_cwd).unwrap();
        assert_eq!(external.origin, PathOrigin::ExplicitExternal);
        assert_eq!(ctx.fs_access.mode, crate::fs_access::FsAccessMode::ReadOnly);
        assert_eq!(ctx.fs_access.workspace.as_deref(), Some(workspace.path()));
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);
    }

    #[test]
    fn workspace_resolver_rejects_parent_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new().with_workspace(binding(workspace.path().to_path_buf()));
        let error = ctx
            .resolve_path(std::path::Path::new("../outside"))
            .unwrap_err();
        assert!(error.to_string().contains("escapes workspace root"));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_resolver_rejects_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link")).unwrap();
        let ctx = ToolCtx::new().with_workspace(binding(workspace.path().to_path_buf()));

        let error = ctx
            .resolve_path(std::path::Path::new("link/file"))
            .unwrap_err();
        assert!(error.to_string().contains("escapes workspace root"));
    }

    #[test]
    fn ordinary_context_keeps_process_cwd_semantics() {
        let process_cwd = std::env::current_dir().unwrap();
        let ctx = ToolCtx::new();
        assert_eq!(ctx.resolve_cwd(None).unwrap(), process_cwd);
        assert_eq!(
            ctx.resolve_path(std::path::Path::new("child")).unwrap(),
            process_cwd.join("child")
        );
    }
}
