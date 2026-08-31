use std::collections::HashMap;

use atman_dsl::ast::{File, FlowDecl};

use crate::error::RuntimeError;
use crate::event::{Event, EventSink, FlowRunId, FlowStatus, TurnId};
use crate::exec::exec_flow_with_siblings;
use crate::invocation_env::InvocationEnv;
use crate::provider::ProviderRegistry;
use crate::provider_lifecycle::ProviderLifecycle;
use crate::session::Session;
use crate::tool::{ToolCtx, ToolRegistry};
use crate::value::Value;

#[derive(Clone, Default)]
pub struct RootInvocation {
    pub turn_id: Option<TurnId>,
    pub session: Option<std::sync::Arc<Session>>,
    pub first_run_id: Option<FlowRunId>,
    pub env: InvocationEnv,
}

#[derive(Clone)]
pub struct Executor {
    pub tools: ToolRegistry,
    pub providers: ProviderRegistry,
    pub events: EventSink,
    pub tool_ctx: ToolCtx,
    pub safety: Option<crate::safety::SafetyConfig>,
    /// When set, relative `@` paths are resolved against this directory.
    pub source_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, thiserror::Error)]
#[error("executor already has a provider lifecycle")]
pub struct ProviderLifecycleAlreadyAttached;

impl Executor {
    pub fn new() -> Self {
        let tools = ToolRegistry::new();
        crate::tools::register_tier_zero(&tools);
        Self {
            tools,
            providers: ProviderRegistry::new(),
            events: EventSink::new(),
            tool_ctx: ToolCtx::new(),
            safety: None,
            source_dir: None,
        }
    }

    pub fn with_events(events: EventSink) -> Self {
        let tools = ToolRegistry::new();
        crate::tools::register_tier_zero(&tools);
        Self {
            tools,
            providers: ProviderRegistry::new(),
            events,
            tool_ctx: ToolCtx::new(),
            safety: None,
            source_dir: None,
        }
    }

    pub fn attach_provider_lifecycle(
        &mut self,
        hub: crate::config_hub::ConfigHub,
    ) -> Result<ProviderLifecycle, ProviderLifecycleAlreadyAttached> {
        self.providers
            .attach_provider_lifecycle(hub)
            .ok_or(ProviderLifecycleAlreadyAttached)
    }

    pub fn provider_lifecycle(&self) -> Option<ProviderLifecycle> {
        self.providers.provider_lifecycle()
    }

    pub fn with_safety(mut self, safety: crate::safety::SafetyConfig) -> Self {
        self.safety = Some(safety);
        self
    }

    pub async fn run(
        &self,
        file: &File,
        flow_name: &str,
        args: Vec<(String, Value)>,
    ) -> Result<Value, RuntimeError> {
        self.run_in_turn(file, flow_name, args, None, None).await
    }

    pub async fn run_in_turn(
        &self,
        file: &File,
        flow_name: &str,
        args: Vec<(String, Value)>,
        turn_id: Option<TurnId>,
        session: Option<std::sync::Arc<Session>>,
    ) -> Result<Value, RuntimeError> {
        self.run_with_invocation(
            file,
            flow_name,
            args,
            RootInvocation {
                turn_id,
                session,
                ..RootInvocation::default()
            },
        )
        .await
    }

    pub async fn run_in_turn_with_env(
        &self,
        file: &File,
        flow_name: &str,
        args: Vec<(String, Value)>,
        turn_id: Option<TurnId>,
        session: Option<std::sync::Arc<Session>>,
        invocation_env: InvocationEnv,
    ) -> Result<Value, RuntimeError> {
        self.run_with_invocation(
            file,
            flow_name,
            args,
            RootInvocation {
                turn_id,
                session,
                env: invocation_env,
                ..RootInvocation::default()
            },
        )
        .await
    }

    pub async fn run_in_turn_with_run_id(
        &self,
        file: &File,
        flow_name: &str,
        args: Vec<(String, Value)>,
        turn_id: Option<TurnId>,
        session: Option<std::sync::Arc<Session>>,
        first_run_id: Option<FlowRunId>,
    ) -> Result<Value, RuntimeError> {
        self.run_with_invocation(
            file,
            flow_name,
            args,
            RootInvocation {
                turn_id,
                session,
                first_run_id,
                ..RootInvocation::default()
            },
        )
        .await
    }

    pub async fn run_with_invocation(
        &self,
        file: &File,
        flow_name: &str,
        args: Vec<(String, Value)>,
        invocation: RootInvocation,
    ) -> Result<Value, RuntimeError> {
        let flows: HashMap<_, _> = file
            .flows
            .iter()
            .map(|f| (f.name.name.clone(), f.clone()))
            .collect();
        let mut current = flow_name.to_string();
        let mut current_args = args;
        let mut next_run_id = invocation.first_run_id.clone();
        for _ in 0..5 {
            let flow = flows
                .get(&current)
                .ok_or_else(|| RuntimeError::UndefinedTool(format!("flow `{current}`")))?;
            match self
                .run_flow(flow, current_args, &flows, &invocation, next_run_id.take())
                .await
            {
                Err(RuntimeError::Redirect(target)) => {
                    current = target;
                    current_args = Vec::new();
                    continue;
                }
                other => return other,
            }
        }
        Err(RuntimeError::ToolFailed(
            "redirect chain exceeded max depth (5)".into(),
        ))
    }

    async fn run_flow(
        &self,
        flow: &FlowDecl,
        args: Vec<(String, Value)>,
        flows: &HashMap<String, FlowDecl>,
        invocation: &RootInvocation,
        run_id: Option<FlowRunId>,
    ) -> Result<Value, RuntimeError> {
        let turn_id = invocation.turn_id.clone();
        let session = invocation.session.clone();
        if let Some(session) = session.as_ref() {
            session.set_tool_output_budget(self.tool_ctx.tool_output_budget);
        }
        let run_id = run_id.unwrap_or_else(FlowRunId::now);
        let task_label = root_task_label(
            &flow.name.name,
            &args,
            session.as_ref().and_then(|session| session.goal()),
        );
        let flow_cancel = session
            .as_ref()
            .map(|s| s.flow_cancel_token())
            .unwrap_or_default();
        let flow_registry = session
            .as_ref()
            .map(|session| std::sync::Arc::clone(&session.flow_registry))
            .or_else(|| self.tool_ctx.flow_registry.clone())
            .unwrap_or_else(|| std::sync::Arc::new(crate::tools::agent_ctrl::FlowRegistry::new()));
        // The broker authenticates against a specific registry, so an inherited
        // broker is only reusable when it is bound to the registry resolved above.
        // Otherwise mint one for this registry, keeping standalone runs (no session)
        // on the same permission pipeline as session-backed runs.
        let permission_broker = session
            .as_ref()
            .map(|session| session.permission_broker())
            .or_else(|| {
                self.tool_ctx
                    .permission_broker
                    .clone()
                    .filter(|broker| broker.is_for_registry(&flow_registry))
            })
            .unwrap_or_else(|| {
                crate::permission::PermissionBroker::shared(std::sync::Arc::clone(&flow_registry))
            });
        let session_id = session
            .as_ref()
            .map(|session| session.id().to_string())
            .or_else(|| self.tool_ctx.session_id.clone())
            .unwrap_or_else(|| format!("standalone-{}", uuid::Uuid::now_v7()));
        let trust = session
            .as_ref()
            .map(|session| session.trust_config())
            .or_else(|| self.tool_ctx.trust.clone())
            .unwrap_or_default();
        let workspace_root = self
            .tool_ctx
            .workspace
            .as_ref()
            .map(|binding| binding.path.clone())
            .or_else(|| self.tool_ctx.fs_access.workspace.clone());
        let identity = flow_registry.register_root(
            session_id.clone(),
            run_id.clone(),
            crate::flow_authority::EffectiveAuthority::root(
                &trust,
                crate::flow_authority::contract_allows_shell(flow.contract.as_ref()),
                workspace_root,
            ),
        )?;
        let task_id = self.tool_ctx.task_registry.as_ref().map(|tr| {
            tr.register_flow_with_run_id(
                task_label,
                run_id.0.to_string(),
                self.tool_ctx
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "anon".into()),
                flow_cancel.clone(),
                None,
                run_id.clone(),
            )
        });
        let _lifecycle_guard = flow_registry.lifecycle_guard(&run_id);
        self.events.emit(Event::FlowStart {
            run_id: run_id.clone(),
            flow_name: flow.name.name.clone(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        });
        if let Some(sess) = session.as_ref() {
            let _ = sess
                .stream_tx()
                .send(crate::stream::StreamFrame::FlowStart {
                    run_id: run_id.0.to_string(),
                    flow_name: flow.name.name.clone(),
                    parent_run_id: None,
                    parent_node_id: None,
                });
        }
        let graph = crate::nodegraph::extract_graph(flow);
        self.events.emit(Event::FlowGraph {
            run_id: run_id.clone(),
            graph: graph.clone(),
        });
        if let Some(sess) = session.as_ref() {
            let _ = sess
                .stream_tx()
                .send(crate::stream::StreamFrame::FlowGraph {
                    run_id: run_id.0.to_string(),
                    graph,
                });
        }
        // Root's tool_ctx carries session stream_tx so emit sites use
        // tool_ctx.stream_tx uniformly.
        let mut tool_ctx = self
            .tool_ctx
            .clone()
            .with_invocation_env(invocation.env.clone());
        tool_ctx.model_tool_exposures = Some(Default::default());
        tool_ctx.flow_registry = Some(std::sync::Arc::clone(&flow_registry));
        tool_ctx.permission_broker = Some(std::sync::Arc::clone(&permission_broker));
        tool_ctx.trust = Some(trust);
        tool_ctx.flow_identity = Some(identity);
        tool_ctx.flow_run_id = Some(run_id.clone());
        tool_ctx.session_id = Some(session_id);
        if let Some(sess) = session.as_ref() {
            tool_ctx.stream_tx = Some(sess.stream_tx());
            tool_ctx.session_messages_handle = Some(sess.messages_handle());
            // Register root so flow.output/interject("root") work. Root's llm
            // context stays on session MessageStream; entry is for output +
            // interjection addressing.
            let root_entry = sess.flow_registry.create_entry(
                "root".to_string(),
                sess.goal().unwrap_or_else(|| flow.name.name.clone()),
                String::new(),
                run_id.clone(),
            );
            tool_ctx.agent_entry = Some(std::sync::Arc::clone(&root_entry));
            sess.set_current_root("root".to_string());
        }
        let exec_fut = exec_flow_with_siblings(
            flow,
            args,
            &self.tools,
            &tool_ctx,
            &self.providers,
            flows,
            Some(&self.events),
            turn_id,
            Some(run_id.clone()),
            session.clone(),
            flow_cancel.clone(),
            self.safety.as_ref(),
            self.source_dir.clone(),
        );
        let result = tokio::select! {
            biased;
            _ = flow_cancel.cancelled() => Err(RuntimeError::Cancelled("flow cancelled by user".into())),
            r = exec_fut => r,
        };
        let result = if let Err(RuntimeError::Cancelled(_)) = &result {
            let suicide = task_id.as_ref().and_then(|id| {
                self.tool_ctx
                    .task_registry
                    .as_ref()
                    .and_then(|tr| tr.lookup(id))
                    .and_then(|snap| snap.termination)
            }) == Some(crate::task_registry::TaskTermination::Suicide);
            if suicide {
                Err(RuntimeError::Cancelled("flow terminated by suicide".into()))
            } else {
                result
            }
        } else {
            result
        };
        let status = match &result {
            Ok(v) => {
                if let Value::Err(e) = v
                    && matches!(e, RuntimeError::Cancelled(_))
                {
                    FlowStatus::Cancelled
                } else {
                    FlowStatus::Ok
                }
            }
            Err(e) => {
                if matches!(e, RuntimeError::Cancelled(_)) {
                    FlowStatus::Cancelled
                } else {
                    FlowStatus::Errored {
                        message: e.to_string(),
                    }
                }
            }
        };
        let cancelled = matches!(status, FlowStatus::Cancelled);
        let suicide = task_id.as_ref().and_then(|id| {
            self.tool_ctx
                .task_registry
                .as_ref()
                .and_then(|tr| tr.lookup(id))
                .and_then(|snap| snap.termination)
        }) == Some(crate::task_registry::TaskTermination::Suicide);
        if let (Some(tr), Some(tid)) = (self.tool_ctx.task_registry.as_ref(), &task_id) {
            let ts = match &status {
                FlowStatus::Ok => crate::task_registry::TaskStatus::Ok,
                FlowStatus::Cancelled => crate::task_registry::TaskStatus::Killed,
                FlowStatus::Errored { .. } => crate::task_registry::TaskStatus::Err,
            };
            tr.finish(tid, ts);
        }
        drop(_lifecycle_guard);
        self.events.emit(Event::FlowEnd {
            run_id: run_id.clone(),
            flow_name: flow.name.name.clone(),
            status: status.clone(),
        });
        if let Some(sess) = session.as_ref() {
            let _ = sess.stream_tx().send(crate::stream::StreamFrame::FlowDone {
                run_id: run_id.0.to_string(),
                flow_name: flow.name.name.clone(),
                ok: matches!(status, FlowStatus::Ok),
                cancelled,
                suicide,
            });
        }
        result
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

fn root_task_label(
    flow_name: &str,
    args: &[(String, Value)],
    session_goal: Option<String>,
) -> String {
    const USER_TEXT_KEYS: &[&str] = &["input", "prompt", "goal", "task", "message", "query"];

    let named_user_text = USER_TEXT_KEYS.iter().find_map(|wanted| {
        args.iter().find_map(|(name, value)| {
            (name == wanted)
                .then_some(value)
                .and_then(|value| match value {
                    Value::Str(value) => crate::task_registry::normalize_task_label(value),
                    _ => None,
                })
        })
    });
    named_user_text
        .or_else(|| {
            args.iter().find_map(|(_, value)| match value {
                Value::Str(value) => crate::task_registry::normalize_task_label(value),
                _ => None,
            })
        })
        .or_else(|| {
            session_goal
                .as_deref()
                .and_then(crate::task_registry::normalize_task_label)
        })
        .unwrap_or_else(|| flow_name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_provider_lifecycle_uses_and_retains_the_executor_registry() {
        let dir = tempfile::tempdir().unwrap();
        let mut executor = Executor::new();
        let lifecycle = executor
            .attach_provider_lifecycle(crate::config_hub::ConfigHub::from_config_dir(dir.path()))
            .unwrap();

        assert!(
            lifecycle
                .provider_registry()
                .shares_storage_with(&executor.providers)
        );
        executor.tool_ctx = ToolCtx::new();
        assert!(executor.provider_lifecycle().is_some());
        let cloned = executor.clone();
        drop(lifecycle);
        drop(executor);
        assert!(cloned.provider_lifecycle().is_some());
    }

    #[test]
    fn provider_lifecycle_rejects_repeated_attachment() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut executor = Executor::new();
        executor
            .attach_provider_lifecycle(crate::config_hub::ConfigHub::from_config_dir(first.path()))
            .unwrap();

        assert!(matches!(
            executor.attach_provider_lifecycle(crate::config_hub::ConfigHub::from_config_dir(
                second.path()
            )),
            Err(ProviderLifecycleAlreadyAttached)
        ));
    }

    #[test]
    fn root_task_title_prefers_named_user_text() {
        let args = vec![
            ("model".into(), Value::Str("gpt-example".into())),
            ("input".into(), Value::Str("  检查   当前项目  ".into())),
        ];

        assert_eq!(
            root_task_label("agent", &args, Some("旧目标".into())),
            "检查 当前项目"
        );
    }

    #[test]
    fn root_task_title_falls_back_to_goal_then_flow_name() {
        assert_eq!(
            root_task_label("agent", &[], Some("  完成   审计 ".into())),
            "完成 审计"
        );
        assert_eq!(root_task_label("agent", &[], None), "agent");
    }

    #[tokio::test]
    async fn root_invocation_projects_tool_output_budget_into_session() {
        let budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 7,
            max_bytes: 777,
            max_line_bytes: 111,
        };
        let mut executor = Executor::new();
        executor.tool_ctx.tool_output_budget = budget;
        let session = std::sync::Arc::new(Session::open_ephemeral());
        let file = atman_dsl::parse::parse_file(
            r#"flow start() -> string {
    return "ok"
}"#,
        )
        .unwrap();

        executor
            .run_in_turn(&file, "start", Vec::new(), None, Some(session.clone()))
            .await
            .unwrap();

        assert_eq!(session.tool_output_budget(), budget);
    }
}
