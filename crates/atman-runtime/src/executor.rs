use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use atman_dsl::ast::{File, FlowDecl};

use crate::error::RuntimeError;
use crate::event::{Event, EventSink, FlowRunId, FlowStatus};
use crate::exec::exec_flow_with_siblings;
use crate::provider::ProviderRegistry;
use crate::tool::{ToolCtx, ToolRegistry};
use crate::value::Value;

pub struct Executor {
    pub tools: ToolRegistry,
    pub providers: ProviderRegistry,
    pub events: EventSink,
    pub tool_ctx: ToolCtx,
    next_run_id: AtomicU64,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            tools: ToolRegistry::new(),
            providers: ProviderRegistry::new(),
            events: EventSink::new(),
            tool_ctx: ToolCtx::new(),
            next_run_id: AtomicU64::new(1),
        }
    }

    pub async fn run(
        &self,
        file: &File,
        flow_name: &str,
        args: Vec<(String, Value)>,
    ) -> Result<Value, RuntimeError> {
        let flows: HashMap<_, _> = file
            .flows
            .iter()
            .map(|f| (f.name.name.clone(), f.clone()))
            .collect();
        let flow = flows
            .get(flow_name)
            .ok_or_else(|| RuntimeError::UndefinedTool(format!("flow `{flow_name}`")))?;
        self.run_flow(flow, args, &flows).await
    }

    async fn run_flow(
        &self,
        flow: &FlowDecl,
        args: Vec<(String, Value)>,
        flows: &HashMap<String, FlowDecl>,
    ) -> Result<Value, RuntimeError> {
        let run_id = FlowRunId(self.next_run_id.fetch_add(1, Ordering::SeqCst));
        self.events.emit(Event::FlowStart {
            run_id: run_id.clone(),
            flow_name: flow.name.name.clone(),
        });
        let result = exec_flow_with_siblings(
            flow,
            args,
            &self.tools,
            &self.tool_ctx,
            &self.providers,
            flows,
            Some(&self.events),
        )
        .await;
        let status = match &result {
            Ok(_) => FlowStatus::Ok,
            Err(e) => FlowStatus::Errored(e.to_string()),
        };
        self.events.emit(Event::FlowEnd {
            run_id,
            flow_name: flow.name.name.clone(),
            status,
        });
        result
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}
