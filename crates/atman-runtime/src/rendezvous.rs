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

    /// Register against the calling invocation, independently of the current view.
    fn register_in_context(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
        _context: &PromptContext,
    ) -> oneshot::Receiver<serde_json::Value> {
        self.register_with_payload(id, kind, payload)
    }
}

/// Trusted host context captured when a tool requests an interaction.
#[derive(Clone)]
pub struct PromptContext {
    pub events: crate::event::EventSink,
    pub session: Option<Arc<crate::Session>>,
    pub run_id: Option<crate::event::FlowRunId>,
    pub tool_use_id: Option<String>,
}

struct ContextResolver {
    resolver: Arc<dyn PromptResolver>,
    context: PromptContext,
}

impl PromptResolver for ContextResolver {
    fn register(&self, id: PromptId) -> oneshot::Receiver<serde_json::Value> {
        self.resolver.register(id)
    }

    fn register_with_payload(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
    ) -> oneshot::Receiver<serde_json::Value> {
        self.resolver
            .register_in_context(id, kind, payload, &self.context)
    }

    fn drop_pending(&self, id: &PromptId) {
        self.resolver.drop_pending(id);
    }

    fn expire_pending(&self, id: &PromptId) -> bool {
        self.resolver.expire_pending(id)
    }
}

impl crate::tool::ToolCtx {
    /// Bind the shared host transport to this invocation before registering a prompt.
    pub fn prompt_resolver(&self) -> Option<Arc<dyn PromptResolver>> {
        self.prompt_resolver.as_ref().map(|resolver| {
            Arc::new(ContextResolver {
                resolver: resolver.clone(),
                context: PromptContext {
                    events: self.events.clone().unwrap_or_default(),
                    session: self
                        .instance_publication
                        .as_ref()
                        .map(|binding| binding.worker.context())
                        .or_else(|| self.session_runtime.clone())
                        .or_else(|| self.context_records_session.clone()),
                    run_id: self.flow_run_id.clone(),
                    tool_use_id: self
                        .tool_use_id
                        .clone()
                        .or_else(|| self.current_node_id.clone()),
                },
            }) as Arc<dyn PromptResolver>
        })
    }
}

struct PromptGuard<'a> {
    resolver: &'a Arc<dyn PromptResolver>,
    id: PromptId,
    retained: bool,
}

impl<'a> PromptGuard<'a> {
    fn new(resolver: &'a Arc<dyn PromptResolver>, id: PromptId) -> Self {
        Self {
            resolver,
            id,
            retained: false,
        }
    }
}

impl Drop for PromptGuard<'_> {
    fn drop(&mut self) {
        if !self.retained {
            self.resolver.drop_pending(&self.id);
        }
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
    let mut guard = PromptGuard::new(resolver, id);
    match tokio::time::timeout(timeout, &mut rx).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(RuntimeError::ToolFailed(format!(
            "prompt {id} channel closed before answer"
        ))),
        Err(_) => {
            if resolver.expire_pending(&id) {
                guard.retained = true;
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
    let mut guard = PromptGuard::new(resolver, id);
    tokio::select! {
        result = tokio::time::timeout(timeout, &mut rx) => match result {
            Ok(Ok(value)) => Ok(Some(value)),
            Ok(Err(_)) => {
                Err(RuntimeError::ToolFailed(format!(
                    "prompt {id} channel closed before answer"
                )))
            }
            Err(_) => {
                if resolver.expire_pending(&id) {
                    guard.retained = true;
                    Ok(None)
                } else {
                    tokio::select! {
                        result = &mut rx => result.map(Some).map_err(|_| {
                            RuntimeError::ToolFailed(format!(
                                "prompt {id} channel closed before answer"
                            ))
                        }),
                        _ = cancel.cancelled() => {
                            Err(RuntimeError::Cancelled(format!("prompt {id} cancelled")))
                        }
                    }
                }
            }
        },
        _ = cancel.cancelled() => {
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
    let _guard = PromptGuard::new(resolver, id);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => Err(RuntimeError::ToolFailed(format!(
            "prompt {id} channel closed before answer"
        ))),
        Err(_) => Err(RuntimeError::ToolFailed(format!(
            "prompt {id} timed out after {}s",
            timeout.as_secs()
        ))),
    }
}

async fn await_prompt_inner_cancel(
    mut rx: oneshot::Receiver<serde_json::Value>,
    resolver: &Arc<dyn PromptResolver>,
    id: PromptId,
    timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, RuntimeError> {
    let _guard = PromptGuard::new(resolver, id);
    tokio::select! {
        result = tokio::time::timeout(timeout, &mut rx) => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => {
                Err(RuntimeError::ToolFailed(format!(
                    "prompt {id} channel closed before answer"
                )))
            }
            Err(_) => {
                Err(RuntimeError::ToolFailed(format!(
                    "prompt {id} timed out after {}s",
                    timeout.as_secs()
                )))
            }
        },
        _ = cancel.cancelled() => {
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

    #[tokio::test]
    async fn dropping_prompt_future_releases_registration_without_polling_cancellation() {
        for expirable in [false, true] {
            let concrete = Arc::new(PendingResolver::new());
            let resolver: Arc<dyn PromptResolver> = concrete.clone();
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut waiting = Box::pin(async {
                if expirable {
                    await_expirable_prompt_with_payload_cancel(
                        &resolver,
                        PromptId::now(),
                        "form_ask",
                        serde_json::Value::Null,
                        std::time::Duration::from_secs(300),
                        &cancel,
                    )
                    .await
                    .map(|_| ())
                } else {
                    await_prompt_with_payload_cancel(
                        &resolver,
                        PromptId::now(),
                        "form_ask",
                        serde_json::Value::Null,
                        std::time::Duration::from_secs(300),
                        &cancel,
                    )
                    .await
                    .map(|_| ())
                }
            });
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            assert!(concrete.sender.lock().unwrap().is_some());
            drop(waiting);
            assert!(concrete.dropped.load(Ordering::SeqCst));
            assert!(concrete.sender.lock().unwrap().is_none());
            assert!(!concrete.expired.load(Ordering::SeqCst));
        }
    }
}
