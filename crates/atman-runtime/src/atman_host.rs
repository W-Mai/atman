use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use atman_rt::ast::FlowDecl;
use tokio_util::sync::CancellationToken;

use crate::event::{EventSink, FlowRunId, TurnId};
use crate::provider::ProviderRegistry;
use crate::safety::SafetyConfig;
use crate::session::Session;
use crate::source_program::{LinkedProgram, ModuleId};
use crate::tool::{ToolCtx, ToolRegistry};

/// Atman services and external effects supplied to the portable expression engine.
#[derive(Clone)]
pub struct AtmanHost<'a> {
    pub tools: &'a ToolRegistry,
    pub tool_ctx: Cow<'a, ToolCtx>,
    pub providers: &'a ProviderRegistry,
    pub flows: &'a HashMap<String, FlowDecl>,
    /// Resolved source graph for cross-file subflow calls.
    pub linked_program: Option<&'a LinkedProgram>,
    pub current_module: Option<ModuleId>,
    pub allows_shell: bool,
    pub events: Option<&'a EventSink>,
    pub turn_id: Option<TurnId>,
    pub flow_run_id: Option<FlowRunId>,
    pub session_runtime: Option<Arc<Session>>,
    pub flow_cancel: CancellationToken,
    pub safety: Option<&'a SafetyConfig>,
    pub current_node_id: Option<String>,
    /// Directory of the .at source file. When set, relative `@` paths
    /// are resolved against this directory instead of the process CWD.
    pub source_dir: Option<PathBuf>,
}

impl AtmanHost<'_> {
    pub fn with_node(&self, node_id: impl Into<String>) -> Self {
        let mut host = self.clone();
        host.current_node_id = Some(node_id.into());
        host
    }
}
