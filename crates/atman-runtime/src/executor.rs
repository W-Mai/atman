use std::collections::HashMap;
use std::sync::Mutex;

use atman_dsl::ast::{File, FlowDecl};

use crate::error::RuntimeError;
use crate::event::{Event, EventSink, FlowRunId, FlowStatus};
use crate::exec::exec_flow_with_siblings;
use crate::provider::{Attachment, ProviderRegistry};
use crate::tool::{ToolCtx, ToolRegistry};
use crate::value::Value;

pub struct Executor {
    pub tools: ToolRegistry,
    pub providers: ProviderRegistry,
    pub events: EventSink,
    pub tool_ctx: ToolCtx,
    pub pending_attachments: Mutex<Vec<Attachment>>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            tools: ToolRegistry::new(),
            providers: ProviderRegistry::new(),
            events: EventSink::new(),
            tool_ctx: ToolCtx::new(),
            pending_attachments: Mutex::new(Vec::new()),
        }
    }

    pub fn with_events(events: EventSink) -> Self {
        Self {
            tools: ToolRegistry::new(),
            providers: ProviderRegistry::new(),
            events,
            tool_ctx: ToolCtx::new(),
            pending_attachments: Mutex::new(Vec::new()),
        }
    }

    pub fn push_attachment(&self, a: Attachment) {
        self.pending_attachments.lock().unwrap().push(a);
    }

    pub fn pending_attachment_count(&self) -> usize {
        self.pending_attachments.lock().unwrap().len()
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
        let run_id = FlowRunId::now();
        self.events.emit(Event::FlowStart {
            run_id: run_id.clone(),
            flow_name: flow.name.name.clone(),
            ts: chrono::Utc::now(),
        });
        let result = exec_flow_with_siblings(
            flow,
            args,
            &self.tools,
            &self.tool_ctx,
            &self.providers,
            flows,
            Some(&self.events),
            Some(&self.pending_attachments),
        )
        .await;
        let status = match &result {
            Ok(_) => FlowStatus::Ok,
            Err(e) => FlowStatus::Errored {
                message: e.to_string(),
            },
        };
        self.events.emit(Event::FlowEnd {
            run_id,
            flow_name: flow.name.name.clone(),
            status,
            ts: chrono::Utc::now(),
        });
        result
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}
