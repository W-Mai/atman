use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::value::Value;

// `dyn Future` is now `Send` — AST Span was replaced with a custom `Copy + Send + Sync`
// struct, removing the `proc_macro2::Span` (which held `Rc<()>`).  This means
// providers, tools, and classifiers can be spawned with `tokio::spawn` instead
// of requiring `spawn_local` + `LocalSet`.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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
    pub turn_id: Option<crate::event::TurnId>,
    pub flow_run_id: Option<crate::event::FlowRunId>,
    pub history_segment: HistorySegment,
    pub event_seq: Option<u64>,
    pub prompt_resolver: Option<std::sync::Arc<dyn crate::rendezvous::PromptResolver>>,
    pub registry: Option<std::sync::Arc<ToolRegistry>>,
    pub sandbox: Option<std::sync::Arc<dyn crate::sandbox::Sandbox>>,
    execution_policy: Option<crate::trust::ExecutionPolicy>,
    pub events: Option<crate::event::EventSink>,
    pub stdout_broadcast: Option<tokio::sync::broadcast::Sender<String>>,
    pub session_messages: Option<std::sync::Arc<Vec<crate::message::Message>>>,
    pub session_messages_handle:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::message::Message>>>>,
    pub session_runtime: Option<std::sync::Arc<crate::session::Session>>,
    pub compact_lock_handle: Option<std::sync::Arc<tokio::sync::Mutex<()>>>,
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
    pub watch_rules: Option<crate::streaming::WatchRules>,
    /// Called when memory.recent_turns is invoked, with the count of returned messages.
    pub on_memory_recent: Option<std::sync::Arc<dyn Fn(u16) + Send + Sync>>,
    pub history_store: Option<std::sync::Arc<dyn crate::history_store::HistoryStore>>,
    pub agent_entry: Option<std::sync::Arc<crate::tools::agent_ctrl::FlowEntry>>,
    pub tool_output_budget: crate::tools::tool_output::ToolOutputBudget,
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

    pub fn with_session_messages_handle(
        mut self,
        handle: std::sync::Arc<std::sync::Mutex<Vec<crate::message::Message>>>,
    ) -> Self {
        self.session_messages_handle = Some(handle);
        self
    }

    pub fn with_session_runtime(
        mut self,
        session: std::sync::Arc<crate::session::Session>,
    ) -> Self {
        self.session_runtime = Some(session);
        self
    }

    pub fn with_compact_lock_handle(
        mut self,
        handle: std::sync::Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        self.compact_lock_handle = Some(handle);
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

    /// Freezes the active trust policy for one tool invocation. Session-backed
    /// calls read the latest snapshot; the resulting context is then shared by
    /// approval, sandbox selection, and execution for that invocation.
    pub fn for_tool_invocation(mut self, tier: Tier) -> Self {
        if let Some(session) = self.session_runtime.as_ref() {
            self.trust = Some(session.trust_config());
        }
        let trust = self.trust.as_ref().cloned().unwrap_or_default();
        let execution_policy = self
            .flow_identity
            .as_ref()
            .map(|identity| {
                identity
                    .effective_authority
                    .constrain_policy(&trust, tier, [])
                    .0
            })
            .unwrap_or_else(|| trust.execution_policy());
        self.execution_policy = Some(execution_policy);
        if tier != Tier::Four || execution_policy == crate::trust::ExecutionPolicy::Unrestricted {
            self.sandbox = None;
        }
        self
    }

    pub(crate) fn execution_policy(&self) -> Option<crate::trust::ExecutionPolicy> {
        self.execution_policy
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
    tools: std::sync::Arc<std::sync::RwLock<HashMap<String, std::sync::Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, tool: std::sync::Arc<dyn Tool>) {
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
    fn invocation_snapshot_tracks_session_trust_and_selects_tier4_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let initial = crate::trust::TrustConfig::default();
        let session = std::sync::Arc::new(
            crate::session::Session::open_with_trust(dir.path(), initial.clone()).unwrap(),
        );
        let sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
            std::sync::Arc::new(crate::sandbox::SandboxExec::new(dir.path()));
        let base = ToolCtx::new()
            .with_trust(crate::trust::TrustConfig {
                mode: crate::trust::TrustMode::Reckless,
                ..crate::trust::TrustConfig::default()
            })
            .with_session_runtime(std::sync::Arc::clone(&session))
            .with_sandbox(sandbox);

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
        assert!(unrestricted.sandbox.is_none());
        assert!(controlled.sandbox.is_some());
    }

    #[test]
    fn invocation_snapshot_only_exposes_sandbox_to_controlled_tier4() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
            std::sync::Arc::new(crate::sandbox::SandboxExec::new(dir.path()));
        let base = ToolCtx::new().with_sandbox(sandbox);

        assert!(
            base.clone()
                .for_tool_invocation(Tier::Three)
                .sandbox
                .is_none()
        );
        assert!(base.for_tool_invocation(Tier::Four).sandbox.is_some());
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
