use std::sync::Arc;

use tokio::sync::oneshot;
use uuid::Uuid;

use crate::error::RuntimeError;

// PromptId lives in atman-proto but runtime can't depend on proto (proto has no runtime deps).
// Represent as Uuid at this trait boundary; adapter in daemon maps to proto::PromptId.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptId(pub Uuid);

impl PromptId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for PromptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub trait PromptResolver: Send + Sync {
    fn register(&self, id: PromptId) -> oneshot::Receiver<serde_json::Value>;
    fn drop_pending(&self, id: &PromptId);

    fn register_with_payload(
        &self,
        id: PromptId,
        _kind: &str,
        _payload: serde_json::Value,
    ) -> oneshot::Receiver<serde_json::Value> {
        self.register(id)
    }
}

// In-proc fallback: prompts are auto-answered by a caller-provided default. Used when
// no daemon is present; the flow author owns the auto-answer contract via tool args.
pub struct AutoResolveResolver {
    pub default: serde_json::Value,
}

impl PromptResolver for AutoResolveResolver {
    fn register(&self, _id: PromptId) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(self.default.clone());
        rx
    }
    fn drop_pending(&self, _id: &PromptId) {}
}

pub async fn await_prompt(
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, RuntimeError> {
    await_prompt_inner(resolver.register(id), resolver, id, timeout).await
}

pub async fn await_prompt_with_payload(
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    kind: &str,
    payload: serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, RuntimeError> {
    await_prompt_inner(
        resolver.register_with_payload(id, kind, payload),
        resolver,
        id,
        timeout,
    )
    .await
}

async fn await_prompt_inner(
    rx: oneshot::Receiver<serde_json::Value>,
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, RuntimeError> {
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => {
            resolver.drop_pending(&id);
            Err(RuntimeError::ToolFailed(format!(
                "prompt {id} channel closed before answer"
            )))
        }
        Err(_) => {
            resolver.drop_pending(&id);
            Err(RuntimeError::ToolFailed(format!(
                "prompt {id} timed out after {}s",
                timeout.as_secs()
            )))
        }
    }
}
