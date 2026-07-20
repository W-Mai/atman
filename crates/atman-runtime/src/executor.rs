use std::collections::HashMap;

use atman_dsl::ast::{File, FlowDecl};

use crate::error::RuntimeError;
use crate::event::{Event, EventSink, FlowRunId, FlowStatus, TurnId};
use crate::exec::exec_flow_with_siblings;
use crate::provider::ProviderRegistry;
use crate::session::Session;
use crate::tool::{ToolCtx, ToolRegistry};
use crate::value::Value;

#[derive(Clone)]
pub struct Executor {
    pub tools: ToolRegistry,
    pub providers: ProviderRegistry,
    pub events: EventSink,
    pub tool_ctx: ToolCtx,
    pub safety: Option<crate::safety::SafetyConfig>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            tools: ToolRegistry::new(),
            providers: ProviderRegistry::new(),
            events: EventSink::new(),
            tool_ctx: ToolCtx::new(),
            safety: None,
        }
    }

    pub fn with_events(events: EventSink) -> Self {
        Self {
            tools: ToolRegistry::new(),
            providers: ProviderRegistry::new(),
            events,
            tool_ctx: ToolCtx::new(),
            safety: None,
        }
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
        self.run_in_turn_with_run_id(file, flow_name, args, turn_id, session, None)
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
        let flows: HashMap<_, _> = file
            .flows
            .iter()
            .map(|f| (f.name.name.clone(), f.clone()))
            .collect();
        let mut current = flow_name.to_string();
        let mut current_args = args;
        let mut next_run_id = first_run_id;
        for _ in 0..5 {
            let flow = flows
                .get(&current)
                .ok_or_else(|| RuntimeError::UndefinedTool(format!("flow `{current}`")))?;
            match self
                .run_flow(
                    flow,
                    current_args,
                    &flows,
                    turn_id.clone(),
                    session.clone(),
                    next_run_id.take(),
                )
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
        turn_id: Option<TurnId>,
        session: Option<std::sync::Arc<Session>>,
        run_id: Option<FlowRunId>,
    ) -> Result<Value, RuntimeError> {
        let run_id = run_id.unwrap_or_else(FlowRunId::now);
        self.events.emit(Event::FlowStart {
            seq: 0,
            run_id: run_id.clone(),
            flow_name: flow.name.name.clone(),
            parent_run_id: None,
            parent_node_id: None,
            ts: chrono::Utc::now(),
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
            seq: 0,
            run_id: run_id.clone(),
            graph: graph.clone(),
            ts: chrono::Utc::now(),
        });
        if let Some(sess) = session.as_ref() {
            let _ = sess
                .stream_tx()
                .send(crate::stream::StreamFrame::FlowGraph {
                    run_id: run_id.0.to_string(),
                    graph,
                });
        }
        let flow_cancel = session
            .as_ref()
            .map(|s| s.flow_cancel_token())
            .unwrap_or_default();
        let exec_fut = exec_flow_with_siblings(
            flow,
            args,
            &self.tools,
            &self.tool_ctx,
            &self.providers,
            flows,
            Some(&self.events),
            turn_id,
            Some(run_id.clone()),
            session.clone(),
            flow_cancel.clone(),
            self.safety.as_ref(),
        );
        let result = tokio::select! {
            biased;
            _ = flow_cancel.cancelled() => Err(RuntimeError::Cancelled("flow cancelled by user".into())),
            r = exec_fut => r,
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
        self.events.emit(Event::FlowEnd {
            seq: 0,
            run_id: run_id.clone(),
            flow_name: flow.name.name.clone(),
            status: status.clone(),
            ts: chrono::Utc::now(),
        });
        if let Some(sess) = session.as_ref() {
            let _ = sess.stream_tx().send(crate::stream::StreamFrame::FlowDone {
                run_id: run_id.0.to_string(),
                flow_name: flow.name.name.clone(),
                ok: matches!(status, FlowStatus::Ok),
                cancelled,
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
