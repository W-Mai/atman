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
    let mut pending = PendingPromptGuard::new(resolver, id);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => {
            pending.resolved();
            Ok(v)
        }
        Ok(Err(_)) => Err(RuntimeError::ToolFailed(format!(
            "prompt {id} channel closed before answer"
        ))),
        Err(_) => Err(RuntimeError::ToolFailed(format!(
            "prompt {id} timed out after {}s",
            timeout.as_secs()
        ))),
    }
}

struct PendingPromptGuard<'a> {
    resolver: &'a Arc<dyn PromptResolver>,
    id: PromptId,
    pending: bool,
}

impl<'a> PendingPromptGuard<'a> {
    fn new(resolver: &'a Arc<dyn PromptResolver>, id: PromptId) -> Self {
        Self {
            resolver,
            id,
            pending: true,
        }
    }

    fn resolved(&mut self) {
        self.pending = false;
    }
}

impl Drop for PendingPromptGuard<'_> {
    fn drop(&mut self) {
        if self.pending {
            self.resolver.drop_pending(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    struct TrackingResolver {
        registered: AtomicBool,
        dropped: AtomicUsize,
        responder: Mutex<Option<oneshot::Sender<serde_json::Value>>>,
    }

    impl TrackingResolver {
        fn pending() -> Arc<Self> {
            Arc::new(Self {
                registered: AtomicBool::new(false),
                dropped: AtomicUsize::new(0),
                responder: Mutex::new(None),
            })
        }
    }

    impl PromptResolver for TrackingResolver {
        fn register(&self, _id: PromptId) -> oneshot::Receiver<serde_json::Value> {
            let (sender, receiver) = oneshot::channel();
            *self.responder.lock().unwrap() = Some(sender);
            self.registered.store(true, Ordering::Release);
            receiver
        }

        fn drop_pending(&self, _id: &PromptId) {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            self.responder.lock().unwrap().take();
        }
    }

    #[tokio::test]
    async fn dropping_wait_unregisters_pending_prompt() {
        let resolver = TrackingResolver::pending();
        let resolver_for_task: Arc<dyn PromptResolver> = resolver.clone();
        let task = tokio::spawn(async move {
            await_prompt(
                &resolver_for_task,
                PromptId::now(),
                std::time::Duration::from_secs(60),
            )
            .await
        });
        while !resolver.registered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }

        task.abort();
        let _ = task.await;

        assert_eq!(resolver.dropped.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn resolved_wait_does_not_unregister_prompt_again() {
        let resolver = TrackingResolver::pending();
        let resolver_for_task: Arc<dyn PromptResolver> = resolver.clone();
        let task = tokio::spawn(async move {
            await_prompt(
                &resolver_for_task,
                PromptId::now(),
                std::time::Duration::from_secs(60),
            )
            .await
        });
        while !resolver.registered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        resolver
            .responder
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(serde_json::json!(true))
            .unwrap();

        assert_eq!(task.await.unwrap().unwrap(), serde_json::json!(true));
        assert_eq!(resolver.dropped.load(Ordering::Acquire), 0);
    }
}
