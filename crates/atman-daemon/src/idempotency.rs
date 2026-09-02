use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use atman_proto::{JsonRpcError, RequestId};
use futures::FutureExt;
use tokio::sync::watch;

const DEFAULT_RETENTION: usize = 4_096;

type CommandOutcome = Result<serde_json::Value, JsonRpcError>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CommandKey {
    principal: Arc<str>,
    request_id: RequestId,
}

struct Entry {
    method: Arc<str>,
    fingerprint: blake3::Hash,
    outcome: watch::Sender<Option<CommandOutcome>>,
}

struct Inner {
    entries: HashMap<CommandKey, Entry>,
    order: VecDeque<CommandKey>,
}

#[derive(Clone)]
pub(crate) struct IdempotencyRegistry {
    inner: Arc<Mutex<Inner>>,
    retention: usize,
}

impl Default for IdempotencyRegistry {
    fn default() -> Self {
        Self::with_retention(DEFAULT_RETENTION)
    }
}

impl IdempotencyRegistry {
    fn with_retention(retention: usize) -> Self {
        assert!(retention > 0, "idempotency retention must be non-zero");
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                order: VecDeque::new(),
            })),
            retention,
        }
    }

    pub async fn execute<P, F>(
        &self,
        principal: impl Into<Arc<str>>,
        request_id: RequestId,
        method: impl Into<Arc<str>>,
        params: &P,
        operation: F,
    ) -> CommandOutcome
    where
        P: serde::Serialize + ?Sized,
        F: Future<Output = CommandOutcome> + Send + 'static,
    {
        let encoded = serde_json::to_vec(params).map_err(|error| {
            JsonRpcError::internal(format!("could not fingerprint command parameters: {error}"))
        })?;
        let fingerprint = blake3::hash(&encoded);
        let key = CommandKey {
            principal: principal.into(),
            request_id,
        };
        let method = method.into();
        let (mut outcome, is_new) = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entry) = inner.entries.get(&key) {
                if entry.method != method || entry.fingerprint != fingerprint {
                    return Err(JsonRpcError::invalid_params(format!(
                        "request_id {} was already used for a different command",
                        key.request_id
                    )));
                }
                (entry.outcome.subscribe(), false)
            } else {
                let (tx, rx) = watch::channel(None);
                inner.entries.insert(
                    key.clone(),
                    Entry {
                        method,
                        fingerprint,
                        outcome: tx,
                    },
                );
                inner.order.push_back(key.clone());
                (rx, true)
            }
        };

        if is_new {
            let registry = self.clone();
            tokio::spawn(async move {
                let result = AssertUnwindSafe(operation)
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err(JsonRpcError::internal("command execution panicked")));
                let sender = registry
                    .inner
                    .lock()
                    .unwrap()
                    .entries
                    .get(&key)
                    .map(|entry| entry.outcome.clone());
                if let Some(sender) = sender {
                    sender.send_replace(Some(result));
                }
                registry.prune_completed();
            });
        }

        loop {
            if let Some(result) = outcome.borrow().clone() {
                self.prune_completed();
                return result;
            }
            if outcome.changed().await.is_err() {
                return Err(JsonRpcError::internal(
                    "idempotent command outcome channel closed",
                ));
            }
        }
    }

    fn prune_completed(&self) {
        let mut inner = self.inner.lock().unwrap();
        let mut remaining = inner.order.len();
        while inner.entries.len() > self.retention && remaining > 0 {
            remaining -= 1;
            let Some(key) = inner.order.pop_front() else {
                break;
            };
            let completed = inner
                .entries
                .get(&key)
                .is_some_and(|entry| entry.outcome.borrow().is_some());
            if completed {
                inner.entries.remove(&key);
            } else {
                inner.order.push_back(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn concurrent_retries_share_one_in_flight_execution() {
        let registry = IdempotencyRegistry::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let request_id = RequestId::now();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let first = {
            let registry = registry.clone();
            let calls = calls.clone();
            let request_id = request_id.clone();
            let release = release.clone();
            tokio::spawn(async move {
                registry
                    .execute(
                        "principal",
                        request_id,
                        "session.submit",
                        &serde_json::json!({"message": "hello"}),
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            let _ = started.send(());
                            release.notified().await;
                            Ok(serde_json::json!({"run_id": "one"}))
                        },
                    )
                    .await
            })
        };
        started_rx.await.unwrap();
        let second = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .execute(
                        "principal",
                        request_id,
                        "session.submit",
                        &serde_json::json!({"message": "hello"}),
                        async {
                            panic!("duplicate operation must not execute");
                        },
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_one();
        assert_eq!(
            first.await.unwrap().unwrap(),
            second.await.unwrap().unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn request_id_reuse_with_different_payload_is_rejected() {
        let registry = IdempotencyRegistry::default();
        let request_id = RequestId::now();
        registry
            .execute(
                "principal",
                request_id.clone(),
                "session.rename",
                &serde_json::json!({"title": "one"}),
                async { Ok(serde_json::json!({"title": "one"})) },
            )
            .await
            .unwrap();
        let error = registry
            .execute(
                "principal",
                request_id,
                "session.rename",
                &serde_json::json!({"title": "two"}),
                async { Ok(serde_json::Value::Null) },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, JsonRpcError::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn completed_entries_are_evicted_in_insertion_order() {
        let registry = IdempotencyRegistry::with_retention(2);
        let first = RequestId::now();
        for request_id in [first.clone(), RequestId::now(), RequestId::now()] {
            registry
                .execute(
                    "principal",
                    request_id,
                    "command",
                    &serde_json::Value::Null,
                    async { Ok(serde_json::Value::Null) },
                )
                .await
                .unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        registry
            .execute("principal", first, "command", &serde_json::Value::Null, {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::Value::Null)
                }
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
