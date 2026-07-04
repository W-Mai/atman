use std::sync::Arc;

use atman_proto::PromptId as ProtoPromptId;
use atman_runtime::event::EventSink;
use atman_runtime::rendezvous::{PromptId as RuntimePromptId, PromptResolver};
use tokio::sync::oneshot;

use crate::DaemonState;

pub struct DaemonPromptResolver {
    pub state: Arc<DaemonState>,
    pub sink: EventSink,
}

impl PromptResolver for DaemonPromptResolver {
    fn register(&self, id: RuntimePromptId) -> oneshot::Receiver<serde_json::Value> {
        self.state.register_pending_prompt(ProtoPromptId(id.0))
    }

    fn register_with_payload(
        &self,
        id: RuntimePromptId,
        kind: &str,
        payload: serde_json::Value,
    ) -> oneshot::Receiver<serde_json::Value> {
        self.state.register_pending_prompt_broadcast(
            ProtoPromptId(id.0),
            kind,
            payload,
            self.sink.clone(),
        )
    }

    fn drop_pending(&self, id: &RuntimePromptId) {
        self.state.drop_pending_prompt(&ProtoPromptId(id.0));
    }
}
