//! Message views and their compaction and cache state share one owner.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::message::Message;
use crate::message_stream::{MessageStream, MessageWindow};
use crate::session::CompactReviewMode;

/// A message context with its own compaction lock, epoch, usage, and prefix observations.
pub struct ContextState {
    pub(crate) messages: Arc<Mutex<Vec<Message>>>,
    stream: Option<MessageStream>,
    pub(crate) compaction: CompactionState,
}

impl ContextState {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages: Arc::new(Mutex::new(messages)),
            stream: None,
            compaction: CompactionState::new(),
        }
    }

    pub(crate) fn from_stream(
        stream: MessageStream,
        messages: Vec<Message>,
        compaction: CompactionState,
    ) -> Self {
        Self {
            messages: Arc::new(Mutex::new(messages)),
            stream: Some(stream),
            compaction,
        }
    }

    pub fn messages(&self) -> MessageWindow {
        match &self.stream {
            Some(stream) => stream.window(),
            None => MessageWindow::from(
                self.messages
                    .lock()
                    .expect("context messages poisoned")
                    .clone(),
            ),
        }
    }

    pub fn messages_handle(&self) -> &Arc<Mutex<Vec<Message>>> {
        &self.messages
    }

    pub(crate) fn messages_full(&self) -> Arc<Vec<Message>> {
        match &self.stream {
            Some(stream) => stream.full_messages(),
            None => Arc::new(
                self.messages
                    .lock()
                    .expect("context messages poisoned")
                    .clone(),
            ),
        }
    }

    pub fn compact_lock(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.compaction.lock
    }

    pub(crate) fn record_call(
        &self,
        provider: &str,
        model: &str,
        call_purpose: crate::context_plan::ContextCallPurpose,
        call_identity: crate::context_plan::ContextCallIdentity,
        record: crate::context_plan::ContextUsageRecord,
    ) {
        let key = crate::context_plan::ContextUsageKey {
            provider: provider.to_string(),
            model: model.to_string(),
            call_purpose,
            call_identity,
        };
        self.compaction
            .last_context_usage
            .lock()
            .expect("context usage lock poisoned")
            .insert(key, record);
    }

    pub fn last_usage(
        &self,
        key: &crate::context_plan::ContextUsageKey,
    ) -> Option<crate::context_plan::ContextUsageRecord> {
        self.compaction
            .last_context_usage
            .lock()
            .expect("context usage lock poisoned")
            .get(key)
    }

    pub(crate) fn epoch(&self) -> Option<String> {
        self.compaction.context_epoch()
    }

    pub(crate) fn update_epoch(&self, messages: &[Message]) {
        self.compaction.update_context_epoch(messages);
    }

    pub(crate) fn observe_prefix(
        &self,
        provider: &str,
        model: &str,
        call_purpose: crate::context_plan::ContextCallPurpose,
        call_identity: crate::context_plan::ContextCallIdentity,
        snapshot: crate::context_plan::ContextPrefixSnapshot,
    ) -> crate::context_plan::ContextCacheObservation {
        self.compaction
            .last_context_prefix
            .lock()
            .expect("context prefix lock poisoned")
            .observe(call_purpose, call_identity, provider, model, snapshot)
    }
}

pub(crate) struct CompactionState {
    pub manual_pending: std::sync::atomic::AtomicBool,
    pub model_window_tokens: std::sync::atomic::AtomicU64,
    pub review_mode: Mutex<CompactReviewMode>,
    pub lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    last_context_usage: Mutex<LastContextUsageStore>,
    last_context_prefix: Mutex<crate::context_plan::ContextPrefixTracker>,
    context_epoch: Mutex<Option<String>>,
}

impl CompactionState {
    pub(crate) fn new() -> Self {
        Self {
            manual_pending: std::sync::atomic::AtomicBool::new(false),
            model_window_tokens: std::sync::atomic::AtomicU64::new(0),
            review_mode: Mutex::new(CompactReviewMode::default()),
            lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            last_context_usage: Mutex::new(LastContextUsageStore::default()),
            last_context_prefix: Mutex::new(crate::context_plan::ContextPrefixTracker::default()),
            context_epoch: Mutex::new(None),
        }
    }

    pub(crate) fn restore_context_epoch(&self, epoch: Option<String>) {
        *self
            .context_epoch
            .lock()
            .expect("context epoch lock poisoned") = epoch;
    }

    pub(crate) fn update_context_epoch(&self, messages: &[Message]) {
        self.restore_context_epoch(Some(checkpoint_epoch_digest(messages)));
    }

    pub(crate) fn context_epoch(&self) -> Option<String> {
        self.context_epoch
            .lock()
            .expect("context epoch lock poisoned")
            .clone()
    }
}

pub(crate) fn checkpoint_epoch_digest(messages: &[Message]) -> String {
    let bytes = serde_json::to_vec(messages).expect("checkpoint messages must serialize");
    format!("blake3:{}", blake3::hash(&bytes).to_hex())
}

const MAX_LAST_CONTEXT_USAGES: usize = 256;

#[derive(Default)]
struct LastContextUsageStore {
    entries: HashMap<crate::context_plan::ContextUsageKey, crate::context_plan::ContextUsageRecord>,
    order: VecDeque<crate::context_plan::ContextUsageKey>,
}

impl LastContextUsageStore {
    fn insert(
        &mut self,
        key: crate::context_plan::ContextUsageKey,
        record: crate::context_plan::ContextUsageRecord,
    ) {
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key.clone());
        self.entries.insert(key, record);
        while self.entries.len() > MAX_LAST_CONTEXT_USAGES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn get(
        &self,
        key: &crate::context_plan::ContextUsageKey,
    ) -> Option<crate::context_plan::ContextUsageRecord> {
        self.entries.get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_writers_acquire_the_context_before_publishing() {
        for writer_kind in ["root", "tool", "record", "injection"] {
            let session = Arc::new(crate::session::Session::open_ephemeral());
            let context = if writer_kind == "root" {
                session.context().clone()
            } else {
                Arc::new(ContextState::new(Vec::new()))
            };
            let sink = session.sink().clone();
            let turn = crate::event::TurnId::now();
            let ctx = crate::tool::ToolCtx::new()
                .with_context(context.clone())
                .with_events(sink.clone())
                .with_anchors(Some(turn.clone()), None, None);
            let queue = crate::injection::InjectionQueue::new(Some(sink.clone()));
            queue.enqueue(crate::injection::Injection::new_pending(
                turn.clone(),
                "steering",
            ));
            let claim = queue.claim_steering(|_| true).unwrap();
            let batch = sink.batch();
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| match writer_kind {
                    "root" => {
                        session.append_message(Message::user_text(turn, "root"), None);
                    }
                    "tool" => {
                        crate::tools::session::append_message_to_context(
                            &ctx,
                            Message::assistant_text(turn, "assistant"),
                        )
                        .unwrap();
                    }
                    "record" => {
                        crate::tools::context::append_context_records(
                            &ctx,
                            turn,
                            [crate::context_plan::ContextRecordSpec::new(
                                "agent.rule.test",
                                crate::context_plan::ContextRecordAuthority::Retrieved,
                                crate::context_plan::ContextRecordRetention::Latest,
                                crate::context_plan::ContextRecordBody::text("rule"),
                            )],
                        )
                        .unwrap();
                    }
                    "injection" => {
                        claim
                            .commit(Some(context.messages_handle()), || {})
                            .unwrap();
                    }
                    _ => unreachable!(),
                });
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let holds_context = loop {
                    if matches!(
                        context.messages.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ) {
                        break true;
                    }
                    if writer.is_finished() || std::time::Instant::now() >= deadline {
                        break false;
                    }
                    std::thread::yield_now();
                };
                // Release the log before joining, including when the lock-order assertion fails.
                drop(batch);
                writer.join().unwrap();
                assert!(
                    holds_context,
                    "{writer_kind} published before acquiring its context"
                );
            });
            let published: Vec<_> = sink
                .snapshot_envelopes()
                .iter()
                .filter_map(|envelope| {
                    envelope
                        .event
                        .context_message()
                        .map(|(message, _)| message.clone())
                })
                .collect();
            assert_eq!(context.messages().to_vec(), published, "{writer_kind}");
            assert_eq!(published.len(), 1);
        }
    }

    #[tokio::test]
    async fn owner_binding_keeps_messages_locks_epochs_and_observations_together() {
        use crate::context_plan::{
            ContextCacheResetReason, ContextCallIdentity, ContextCallPurpose,
            ContextPrefixSnapshot, ContextUsageKey, ContextUsageRecord,
        };
        let session = Arc::new(crate::session::Session::open_ephemeral());
        let root = crate::tool::ToolCtx::new().with_session_runtime(session.clone());
        let child_state = Arc::new(ContextState::new(Vec::new()));
        let child = root.clone().with_context(child_state.clone());
        let inline = child.clone();
        assert!(Arc::ptr_eq(root.context().unwrap(), session.context()));
        assert!(Arc::ptr_eq(inline.context().unwrap(), &child_state));
        assert!(child.session_runtime().is_none());
        let _guard = child_state.compact_lock().lock().await;
        assert!(inline.context().unwrap().compact_lock().try_lock().is_err());
        assert!(root.context().unwrap().compact_lock().try_lock().is_ok());

        let messages = vec![Message::user_text(crate::event::TurnId::now(), "child")];
        *child_state.messages.lock().unwrap() = messages.clone();
        child_state.update_epoch(&messages);
        assert_eq!(inline.context().unwrap().messages().to_vec(), messages);
        assert!(root.context().unwrap().messages().is_empty());
        assert_eq!(
            inline.context().unwrap().epoch(),
            Some(checkpoint_epoch_digest(&messages))
        );
        assert_eq!(root.context().unwrap().epoch(), None);

        // Identical observation keys must remain isolated by the context owner.
        let identity = ContextCallIdentity::detached();
        let purpose = ContextCallPurpose::General;
        let record = ContextUsageRecord {
            plan_id: crate::context_plan::ContextPlanId::now(),
            usage: crate::provider::TokenUsage::default(),
        };
        let key = ContextUsageKey {
            provider: "provider".into(),
            model: "model".into(),
            call_purpose: purpose,
            call_identity: identity.clone(),
        };
        child_state.record_call(
            "provider",
            "model",
            purpose,
            identity.clone(),
            record.clone(),
        );
        assert_eq!(
            inline.context().unwrap().last_usage(&key).unwrap().plan_id,
            record.plan_id
        );
        assert!(root.context().unwrap().last_usage(&key).is_none());
        let prefix = ContextPrefixSnapshot::provider_neutral(&crate::provider::LlmRequest {
            model: "model".into(),
            messages,
            system: Some("stable".into()),
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: true,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 120,
        })
        .unwrap();
        assert_eq!(
            child_state
                .observe_prefix(
                    "provider",
                    "model",
                    purpose,
                    identity.clone(),
                    prefix.clone()
                )
                .reset_reason,
            Some(ContextCacheResetReason::ColdStart)
        );
        assert_eq!(
            inline
                .context()
                .unwrap()
                .observe_prefix(
                    "provider",
                    "model",
                    purpose,
                    identity.clone(),
                    prefix.clone()
                )
                .reset_reason,
            None
        );
        assert_eq!(
            root.context()
                .unwrap()
                .observe_prefix("provider", "model", purpose, identity, prefix)
                .reset_reason,
            Some(ContextCacheResetReason::ColdStart)
        );
        let rebound = child.with_session_runtime(session);
        assert!(Arc::ptr_eq(
            rebound.context().unwrap(),
            root.context().unwrap()
        ));
    }

    #[test]
    fn last_context_usage_store_evicts_the_oldest_identity() {
        let mut store = LastContextUsageStore::default();
        for index in 0..=MAX_LAST_CONTEXT_USAGES {
            store.insert(
                crate::context_plan::ContextUsageKey {
                    provider: "provider".into(),
                    model: format!("model-{index}"),
                    call_purpose: crate::context_plan::ContextCallPurpose::General,
                    call_identity: crate::context_plan::ContextCallIdentity::detached(),
                },
                crate::context_plan::ContextUsageRecord {
                    plan_id: crate::context_plan::ContextPlanId::now(),
                    usage: crate::provider::TokenUsage::default(),
                },
            );
        }

        assert_eq!(store.entries.len(), MAX_LAST_CONTEXT_USAGES);
        assert!(!store.entries.keys().any(|key| key.model == "model-0"));
        assert!(
            store
                .entries
                .keys()
                .any(|key| key.model == format!("model-{MAX_LAST_CONTEXT_USAGES}"))
        );
    }
}
