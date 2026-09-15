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

    fn expire_pending(&self, id: &PromptId) -> bool {
        self.drop_pending(id);
        true
    }

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

pub async fn await_prompt_with_payload_cancel(
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    kind: &str,
    payload: serde_json::Value,
    timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, RuntimeError> {
    await_prompt_inner_cancel(
        resolver.register_with_payload(id, kind, payload),
        resolver,
        id,
        timeout,
        cancel,
    )
    .await
}

pub async fn await_expirable_prompt_with_payload(
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    kind: &str,
    payload: serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, RuntimeError> {
    let mut rx = resolver.register_with_payload(id, kind, payload);
    match tokio::time::timeout(timeout, &mut rx).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => {
            resolver.drop_pending(&id);
            Err(RuntimeError::ToolFailed(format!(
                "prompt {id} channel closed before answer"
            )))
        }
        Err(_) => {
            if resolver.expire_pending(&id) {
                Err(RuntimeError::ToolFailed(format!(
                    "prompt {id} timed out after {}s",
                    timeout.as_secs()
                )))
            } else {
                rx.await.map_err(|_| {
                    RuntimeError::ToolFailed(format!("prompt {id} channel closed before answer"))
                })
            }
        }
    }
}

pub async fn await_expirable_prompt_with_payload_cancel(
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    kind: &str,
    payload: serde_json::Value,
    timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<serde_json::Value>, RuntimeError> {
    let mut rx = resolver.register_with_payload(id, kind, payload);
    tokio::select! {
        result = tokio::time::timeout(timeout, &mut rx) => match result {
            Ok(Ok(value)) => Ok(Some(value)),
            Ok(Err(_)) => {
                resolver.drop_pending(&id);
                Err(RuntimeError::ToolFailed(format!(
                    "prompt {id} channel closed before answer"
                )))
            }
            Err(_) => {
                if resolver.expire_pending(&id) {
                    Ok(None)
                } else {
                    tokio::select! {
                        result = &mut rx => result.map(Some).map_err(|_| {
                            RuntimeError::ToolFailed(format!(
                                "prompt {id} channel closed before answer"
                            ))
                        }),
                        _ = cancel.cancelled() => {
                            resolver.drop_pending(&id);
                            Err(RuntimeError::Cancelled(format!("prompt {id} cancelled")))
                        }
                    }
                }
            }
        },
        _ = cancel.cancelled() => {
            resolver.drop_pending(&id);
            Err(RuntimeError::Cancelled(format!("prompt {id} cancelled")))
        }
    }
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

async fn await_prompt_inner_cancel(
    mut rx: oneshot::Receiver<serde_json::Value>,
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, RuntimeError> {
    tokio::select! {
        result = tokio::time::timeout(timeout, &mut rx) => match result {
            Ok(Ok(value)) => Ok(value),
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
        },
        _ = cancel.cancelled() => {
            resolver.drop_pending(&id);
            Err(RuntimeError::Cancelled(format!("prompt {id} cancelled")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PendingResolver {
        sender: Mutex<Option<oneshot::Sender<serde_json::Value>>>,
        dropped: AtomicBool,
        expired: AtomicBool,
    }

    impl PendingResolver {
        fn new() -> Self {
            Self {
                sender: Mutex::new(None),
                dropped: AtomicBool::new(false),
                expired: AtomicBool::new(false),
            }
        }
    }

    impl PromptResolver for PendingResolver {
        fn register(&self, _id: PromptId) -> oneshot::Receiver<serde_json::Value> {
            let (tx, rx) = oneshot::channel();
            *self.sender.lock().unwrap() = Some(tx);
            rx
        }

        fn drop_pending(&self, _id: &PromptId) {
            self.dropped.store(true, Ordering::SeqCst);
            self.sender.lock().unwrap().take();
        }

        fn expire_pending(&self, _id: &PromptId) -> bool {
            self.expired.store(true, Ordering::SeqCst);
            self.sender.lock().unwrap().take();
            true
        }
    }

    #[tokio::test]
    async fn expirable_prompt_timeout_is_not_a_tool_error() {
        let concrete = Arc::new(PendingResolver::new());
        let resolver: Arc<dyn PromptResolver> = concrete.clone();
        let result = await_expirable_prompt_with_payload_cancel(
            &resolver,
            PromptId::now(),
            "form_ask",
            serde_json::Value::Null,
            std::time::Duration::from_millis(1),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(result.is_none());
        assert!(concrete.expired.load(Ordering::SeqCst));
        assert!(!concrete.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelling_prompt_drops_it_immediately() {
        let concrete = Arc::new(PendingResolver::new());
        let resolver: Arc<dyn PromptResolver> = concrete.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let result = await_prompt_with_payload_cancel(
            &resolver,
            PromptId::now(),
            "form_ask",
            serde_json::Value::Null,
            std::time::Duration::from_secs(300),
            &cancel,
        )
        .await;

        assert!(matches!(result, Err(RuntimeError::Cancelled(_))));
        assert!(concrete.dropped.load(Ordering::SeqCst));
    }
}
