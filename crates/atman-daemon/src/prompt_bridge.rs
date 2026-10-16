use std::sync::{Arc, Weak};

use atman_proto::PromptId as ProtoPromptId;
use atman_runtime::rendezvous::{PromptContext, PromptId as RuntimePromptId, PromptResolver};
use tokio::sync::oneshot;

use crate::DaemonState;

pub struct DaemonPromptResolver {
    state: Weak<DaemonState>,
}

impl DaemonPromptResolver {
    pub fn new(state: &Arc<DaemonState>) -> Self {
        Self {
            state: Arc::downgrade(state),
        }
    }
}

impl PromptResolver for DaemonPromptResolver {
    fn register(&self, id: RuntimePromptId) -> oneshot::Receiver<serde_json::Value> {
        self.state
            .upgrade()
            .map(|state| state.register_pending_prompt(ProtoPromptId(id.0)))
            .unwrap_or_else(|| oneshot::channel().1)
    }

    fn register_in_context(
        &self,
        id: RuntimePromptId,
        kind: &str,
        payload: serde_json::Value,
        context: &PromptContext,
    ) -> oneshot::Receiver<serde_json::Value> {
        let Some(state) = self.state.upgrade() else {
            return oneshot::channel().1;
        };
        state.register_pending_prompt_broadcast_with_session(
            ProtoPromptId(id.0),
            kind,
            payload,
            context.events.clone(),
            context.session.as_deref(),
        )
    }

    fn drop_pending(&self, id: &RuntimePromptId) {
        if let Some(state) = self.state.upgrade() {
            state.drop_pending_prompt(&ProtoPromptId(id.0));
        }
    }

    fn expire_pending(&self, id: &RuntimePromptId) -> bool {
        self.state
            .upgrade()
            .is_some_and(|state| state.expire_pending_prompt(&ProtoPromptId(id.0)))
    }
}
