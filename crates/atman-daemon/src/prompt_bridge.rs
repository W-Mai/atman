use std::sync::Arc;

use atman_proto::PromptId as ProtoPromptId;
use atman_runtime::rendezvous::{PromptContext, PromptId as RuntimePromptId, PromptResolver};
use tokio::sync::oneshot;

use crate::DaemonState;

pub struct DaemonPromptResolver {
    pub state: Arc<DaemonState>,
}

impl PromptResolver for DaemonPromptResolver {
    fn register(&self, id: RuntimePromptId) -> oneshot::Receiver<serde_json::Value> {
        self.state.register_pending_prompt(ProtoPromptId(id.0))
    }

    fn register_in_context(
        &self,
        id: RuntimePromptId,
        kind: &str,
        payload: serde_json::Value,
        context: &PromptContext,
    ) -> oneshot::Receiver<serde_json::Value> {
        self.state.register_pending_prompt_broadcast_with_session(
            ProtoPromptId(id.0),
            kind,
            payload,
            context.events.clone(),
            context.session.as_deref(),
        )
    }

    fn drop_pending(&self, id: &RuntimePromptId) {
        self.state.drop_pending_prompt(&ProtoPromptId(id.0));
    }

    fn expire_pending(&self, id: &RuntimePromptId) -> bool {
        self.state.expire_pending_prompt(&ProtoPromptId(id.0))
    }
}
