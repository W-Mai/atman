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
    sink: Option<crate::event::EventSink>,
    pub(crate) compaction: CompactionState,
}

impl ContextState {
    /// Binds canonical history publication independently of tool diagnostics.
    pub fn new(mut messages: Vec<Message>, sink: Option<crate::event::EventSink>) -> Self {
        for message in &mut messages {
            message.ensure_part_ids();
        }
        Self {
            messages: Arc::new(Mutex::new(messages)),
            stream: None,
            sink,
            compaction: CompactionState::new(),
        }
    }

    pub(crate) fn from_stream(
        stream: MessageStream,
        compaction: CompactionState,
        sink: crate::event::EventSink,
    ) -> Self {
        stream.assert_bound_to(&sink);
        let messages = stream.window().to_vec();
        Self {
            messages: Arc::new(Mutex::new(messages)),
            stream: Some(stream),
            sink: Some(sink),
            compaction,
        }
    }

    /// Captures a journal-backed source without replaying historical events.
    /// The new owner retains its own messages, compaction lock, and observations.
    pub fn fork(&self, inheritance: crate::event::ContextInheritance) -> std::io::Result<Self> {
        let (Some(stream), Some(sink)) = (&self.stream, self.sink()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "context fork requires a journal-backed message view",
            ));
        };
        let _messages = self.messages.lock().expect("context messages poisoned");
        let (stream, sink) = stream.fork(sink, inheritance);
        let compaction = CompactionState {
            manual_pending: std::sync::atomic::AtomicBool::new(false),
            model_window_tokens: std::sync::atomic::AtomicU64::new(
                self.compaction
                    .model_window_tokens
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            review_mode: Mutex::new(
                *self
                    .compaction
                    .review_mode
                    .lock()
                    .expect("review mode poisoned"),
            ),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            last_context_usage: Mutex::new(
                self.compaction
                    .last_context_usage
                    .lock()
                    .expect("context usage lock poisoned")
                    .clone(),
            ),
            last_context_prefix: Mutex::new(
                self.compaction
                    .last_context_prefix
                    .lock()
                    .expect("context prefix lock poisoned")
                    .clone(),
            ),
            context_epoch: Mutex::new(self.epoch()),
            last_compact_at: Mutex::new(*self.compaction.last_compact_at.lock().unwrap()),
        };
        Ok(Self::from_stream(stream, compaction, sink))
    }

    pub fn context_id(&self) -> Option<&crate::event::ContextId> {
        self.sink().and_then(crate::event::EventSink::context_id)
    }

    /// Returns the journal sink carrying this context's identity.
    pub fn event_sink(&self) -> Option<&crate::event::EventSink> {
        self.sink.as_ref()
    }

    pub(crate) fn sink(&self) -> Option<&crate::event::EventSink> {
        self.event_sink()
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

    pub fn compaction_cooldown_elapsed(&self) -> bool {
        self.compaction
            .last_compact_at
            .lock()
            .unwrap()
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(60))
    }

    pub fn compact_lock(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.compaction.lock
    }

    pub(crate) fn request_manual_compact(&self) {
        self.compaction
            .manual_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn take_manual_compact_request(&self) -> bool {
        self.compaction
            .manual_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Publishes only against an unchanged source, while holding the message lock.
    pub(crate) fn commit_compaction(
        &self,
        expected: &[Message],
        result: &crate::compaction::ContextCompactResult,
        publish: impl FnOnce(),
    ) -> bool {
        let mut messages = self.messages.lock().expect("context messages poisoned");
        if messages.as_slice() != expected {
            return false;
        }
        self.update_epoch(&result.checkpoint_messages);
        self.compaction
            .model_window_tokens
            .store(result.after_tokens, std::sync::atomic::Ordering::Relaxed);
        publish();
        *messages = result.checkpoint_messages.clone();
        *self.compaction.last_compact_at.lock().unwrap() = Some(std::time::Instant::now());
        true
    }

    pub(crate) fn degrade_attachment(
        &self,
        part_id: crate::message::MessagePartId,
        reason: &str,
        turn_id: Option<crate::event::TurnId>,
        flow_run_id: Option<crate::event::FlowRunId>,
    ) -> Option<crate::message::AttachmentPatch> {
        let mut messages = self.messages.lock().expect("context messages poisoned");
        let source = messages
            .iter()
            .flat_map(|message| &message.parts)
            .find_map(|part| match part {
                crate::message::MessagePart::Image {
                    id: Some(id),
                    source,
                } if *id == part_id => Some(source),
                _ => None,
            })?;
        let patch = crate::message::AttachmentPatch {
            target: crate::message::AttachmentTarget::Part { part_id },
            file_basename: crate::attachment_store::display_name(source),
            reason: reason.into(),
        };
        if let Some(sink) = self.sink() {
            sink.emit(crate::event::Event::AttachmentDegraded {
                turn_id,
                flow_run_id,
                patch: patch.clone(),
            });
        }
        for message in messages.iter_mut() {
            patch.apply(0, message);
        }
        Some(patch)
    }

    pub(crate) fn record_call(
        &self,
        provider: &str,
        model: &str,
        call_purpose: crate::context_plan::ContextCallPurpose,
        call_identity: crate::context_plan::ContextCallIdentity,
        record: crate::context_plan::ContextUsageRecord,
    ) {
        self.compaction
            .record_call(provider, model, call_purpose, call_identity, record);
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
        managed_context: bool,
        snapshot: crate::context_plan::ContextPrefixSnapshot,
    ) -> crate::context_plan::ContextCacheObservation {
        self.compaction
            .last_context_prefix
            .lock()
            .expect("context prefix lock poisoned")
            .observe(
                call_purpose,
                call_identity,
                provider,
                model,
                managed_context,
                snapshot,
            )
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
    last_compact_at: Mutex<Option<std::time::Instant>>,
}

impl CompactionState {
    pub(crate) fn record_call(
        &self,
        provider: &str,
        model: &str,
        call_purpose: crate::context_plan::ContextCallPurpose,
        call_identity: crate::context_plan::ContextCallIdentity,
        record: crate::context_plan::ContextUsageRecord,
    ) {
        let input_tokens = record.window_input_tokens();
        if call_purpose == crate::context_plan::ContextCallPurpose::General && input_tokens > 0 {
            self.model_window_tokens
                .store(input_tokens, std::sync::atomic::Ordering::Relaxed);
        }
        let key = crate::context_plan::ContextUsageKey {
            provider: provider.to_string(),
            model: model.to_string(),
            call_purpose,
            call_identity,
        };
        self.last_context_usage
            .lock()
            .expect("context usage lock poisoned")
            .insert(key, record);
    }

    pub(crate) fn new() -> Self {
        Self {
            manual_pending: std::sync::atomic::AtomicBool::new(false),
            model_window_tokens: std::sync::atomic::AtomicU64::new(0),
            review_mode: Mutex::new(CompactReviewMode::default()),
            lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            last_context_usage: Mutex::new(LastContextUsageStore::default()),
            last_context_prefix: Mutex::new(crate::context_plan::ContextPrefixTracker::default()),
            context_epoch: Mutex::new(None),
            last_compact_at: Mutex::new(None),
        }
    }

    pub(crate) fn from_replay<'a>(
        selection: &crate::projection::context::ContextSelection,
        events: impl Iterator<Item = &'a crate::event::EventEnvelope> + Clone,
    ) -> (Self, u64) {
        use crate::event::Event;
        let state = Self::new();
        let mut state_seq = 0;
        let mut checkpoint = None;
        let ownership = crate::projection::message_window::FlowOwnership::from_events(
            events.clone().map(|envelope| &envelope.event),
        );
        for envelope in events.filter(|envelope| selection.includes(envelope)) {
            match &envelope.event {
                Event::ContextHeadSelected { .. } => state_seq = envelope.seq,
                Event::LlmCall {
                    provider,
                    model,
                    context_plan_id: Some(plan_id),
                    managed_context,
                    context_call_purpose: Some(purpose),
                    context_call_identity: Some(identity),
                    usage,
                    ..
                } if managed_context.unwrap_or(true)
                    && (envelope.context_id.is_some()
                        || identity.scope == crate::context_plan::ContextCallScope::Root) =>
                {
                    if *purpose == crate::context_plan::ContextCallPurpose::General {
                        state_seq = envelope.seq;
                    }
                    state.record_call(
                        provider,
                        model,
                        *purpose,
                        identity.clone(),
                        crate::context_plan::ContextUsageRecord {
                            plan_id: plan_id.clone(),
                            usage: usage.clone(),
                        },
                    );
                }
                Event::Checkpoint {
                    flow_run_id,
                    window_tokens,
                    messages,
                    ..
                } if envelope.context_id.is_some()
                    || crate::projection::message_window::message_belongs_to_root(
                        flow_run_id.as_ref(),
                        &ownership.spawned,
                    ) =>
                {
                    state_seq = envelope.seq;
                    checkpoint = Some(messages);
                    state
                        .model_window_tokens
                        .store(*window_tokens, std::sync::atomic::Ordering::Relaxed);
                }
                _ => {}
            }
        }
        if let Some(messages) = checkpoint {
            state.update_context_epoch(messages);
        }
        (state, state_seq)
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
    let content = messages.iter().map(Message::content).collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&content).expect("checkpoint messages must serialize");
    format!("blake3:{}", blake3::hash(&bytes).to_hex())
}

const MAX_LAST_CONTEXT_USAGES: usize = 256;

#[derive(Clone, Default)]
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

    #[tokio::test]
    async fn journal_forks_match_replay_before_and_after_session_restore() {
        use crate::event::{ContextBase, ContextInheritance, TurnId};
        use crate::message::{ImageData, ImageSource, MessagePart};
        for restored in [false, true] {
            for inheritance in [
                ContextInheritance::Full,
                ContextInheritance::CompleteToolPairs,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let mut session = crate::Session::open(dir.path()).unwrap();
                let mut user = Message::user_text(TurnId::now(), "x".repeat(20_000));
                user.parts.push(MessagePart::Image {
                    id: None,
                    source: ImageSource {
                        media_type: "image/png".into(),
                        data: ImageData::Base64 {
                            data: "AA==".into(),
                        },
                        detail: Default::default(),
                    },
                });
                session.append_message(user, None);
                let mut assistant = Message::assistant_text(TurnId::now(), "pending tool");
                assistant.parts.push(MessagePart::ToolUse {
                    id: "pending".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path": "file.txt"}),
                    intent: None,
                });
                session.append_message(assistant, None);
                let original = session.messages();
                let mut replacement = original.to_vec();
                replacement[0].parts[0] = MessagePart::Text {
                    text: "retained user".into(),
                };
                session
                    .commit_rewritten_window(&session.context(), replacement, 10_000, &original, 1)
                    .unwrap();
                if restored {
                    let id = session.id().to_string();
                    session.flush_writer().await.unwrap();
                    session.shutdown().await;
                    session = crate::Session::open_existing(dir.path(), &id).unwrap();
                    assert!(session.sink().snapshot_envelopes().is_empty());
                }
                let source = session.context();
                let identity = crate::context_plan::ContextCallIdentity::detached();
                let purpose = crate::context_plan::ContextCallPurpose::General;
                let usage_key = crate::context_plan::ContextUsageKey {
                    provider: "provider".into(),
                    model: "model".into(),
                    call_purpose: purpose,
                    call_identity: identity.clone(),
                };
                let usage = crate::context_plan::ContextUsageRecord {
                    plan_id: crate::context_plan::ContextPlanId::now(),
                    usage: crate::provider::TokenUsage {
                        input: 1234,
                        ..Default::default()
                    },
                };
                source.record_call(
                    "provider",
                    "model",
                    purpose,
                    identity.clone(),
                    usage.clone(),
                );
                let prefix = crate::context_plan::ContextPrefixSnapshot::provider_neutral(
                    &crate::provider::LlmRequest {
                        model: "model".into(),
                        messages: source.messages().to_vec(),
                        system: Some("stable".into()),
                        input: crate::Value::Unit,
                        schema: None,
                        cache_prompt: true,
                        prompt_cache_key: None,
                        tools: Vec::new(),
                        reasoning: crate::provider::ReasoningSelection::ProviderDefault,
                        stall_timeout_secs: 120,
                    },
                )
                .unwrap();
                source.observe_prefix(
                    "provider",
                    "model",
                    purpose,
                    identity.clone(),
                    true,
                    prefix.clone(),
                );
                let source_raw = source.messages_full();
                let mut expected = source.messages().to_vec();
                if inheritance == ContextInheritance::CompleteToolPairs {
                    crate::message::retain_complete_tool_pairs(&mut expected);
                }
                let cutoff = session.sink().published_seq();
                let count = session.sink().snapshot_envelopes().len();
                source.request_manual_compact();
                let active_request = source.compact_lock().lock().await;
                let child = Arc::new(source.fork(inheritance).unwrap());
                drop(active_request);
                assert_eq!(session.sink().snapshot_envelopes().len(), count + 1);
                let created = session.sink().snapshot_envelopes().pop().unwrap();
                assert_eq!(created.context_id.as_ref(), child.context_id());
                assert!(
                    matches!(created.event, crate::event::Event::ContextCreated {
                    base: Some(ContextBase::LegacyRoot { through_seq }), inheritance: selected,
                } if through_seq == cutoff && selected == inheritance)
                );
                assert_eq!(child.messages().to_vec(), expected);
                assert_eq!(*child.messages_full(), *source_raw);
                assert_eq!(child.epoch(), source.epoch());
                assert!(!Arc::ptr_eq(
                    child.messages_handle(),
                    source.messages_handle()
                ));
                assert!(!Arc::ptr_eq(child.compact_lock(), source.compact_lock()));
                assert!(!child.take_manual_compact_request());
                assert!(source.take_manual_compact_request());
                assert_eq!(child.last_usage(&usage_key), Some(usage.clone()));
                assert_eq!(
                    child
                        .compaction
                        .model_window_tokens
                        .load(std::sync::atomic::Ordering::Relaxed),
                    1234
                );
                assert_eq!(
                    child
                        .observe_prefix(
                            "provider",
                            "model",
                            purpose,
                            identity.clone(),
                            true,
                            prefix.clone()
                        )
                        .reset_reason,
                    None
                );
                *child.compaction.last_context_prefix.lock().unwrap() = Default::default();
                *child.compaction.last_context_usage.lock().unwrap() = Default::default();
                assert_eq!(source.last_usage(&usage_key), Some(usage));
                assert_eq!(
                    source
                        .observe_prefix("provider", "model", purpose, identity, true, prefix)
                        .reset_reason,
                    None
                );

                session
                    .append_message(Message::user_text(TurnId::now(), "late parent input"), None);
                assert_eq!(child.messages().to_vec(), expected);
                let child_ctx = crate::ToolCtx::new().with_context(child.clone());
                crate::tools::session::append_message_to_context(
                    &child_ctx,
                    Message::user_text(TurnId::now(), "child input"),
                )
                .unwrap();
                let image_id = expected
                    .iter()
                    .flat_map(|message| &message.parts)
                    .find_map(|part| {
                        if let MessagePart::Image { id, .. } = part {
                            *id
                        } else {
                            None
                        }
                    })
                    .unwrap();
                child
                    .degrade_attachment(image_id, "invalid_image", None, None)
                    .unwrap();
                assert!(
                    source
                        .messages_full()
                        .iter()
                        .flat_map(|message| &message.parts)
                        .any(|part| matches!(part, MessagePart::Image { .. }))
                );
                let grandchild = Arc::new(child.fork(ContextInheritance::Full).unwrap());
                assert_eq!(grandchild.messages().to_vec(), child.messages().to_vec());
                crate::tools::session::append_message_to_context(
                    &child_ctx,
                    Message::assistant_text(TurnId::now(), "late child output"),
                )
                .unwrap();
                assert_ne!(grandchild.messages().to_vec(), child.messages().to_vec());
                let current = child.messages();
                session
                    .commit_rewritten_window(
                        &child,
                        vec![Message::user_text(TurnId::now(), "short")],
                        100,
                        &current,
                        1,
                    )
                    .unwrap();
                session.flush_writer().await.unwrap();
                let bundle = crate::event_log::replay::SessionReplay::from_path(
                    &session.dir().join("events.jsonl"),
                    None,
                )
                .unwrap();
                for context in [&child, &grandchild] {
                    let replay = crate::projection::context::replay_context(
                        &bundle.events,
                        &ContextBase::Context {
                            context_id: context.context_id().unwrap().clone(),
                            through_seq: session.sink().published_seq(),
                        },
                    )
                    .unwrap();
                    assert_eq!(
                        replay
                            .window()
                            .iter()
                            .map(|(_, message)| message.clone())
                            .collect::<Vec<_>>(),
                        context.messages().to_vec()
                    );
                    assert_eq!(
                        replay
                            .raw
                            .iter()
                            .map(|(_, message)| message.clone())
                            .collect::<Vec<_>>(),
                        *context.messages_full()
                    );
                    assert_eq!(replay.checkpoint_epoch, context.epoch());
                }
                assert_eq!(
                    bundle
                        .view
                        .raw
                        .iter()
                        .map(|(_, message)| message.clone())
                        .collect::<Vec<_>>(),
                    *source.messages_full()
                );
                session.shutdown().await;
            }
        }
        let context = ContextState::new(Vec::new(), Some(crate::event::EventSink::new()));
        assert!(context.fork(ContextInheritance::Full).is_err());
        assert!(context.sink().unwrap().snapshot_envelopes().is_empty());
    }

    #[test]
    fn image_ids_match_live_handles_checkpoints_and_replay() {
        use crate::event::{ContextBase, ContextId, Event, TurnId};
        use crate::message::MessagePart;
        for detached in [false, true] {
            let session = Arc::new(crate::session::Session::open_ephemeral());
            let context_id = ContextId::now();
            let sink = session.sink().clone().with_context(context_id.clone());
            sink.emit(Event::ContextCreated {
                base: None,
                inheritance: crate::event::ContextInheritance::Full,
            });
            let context = Arc::new(ContextState::new(Vec::new(), Some(sink.clone())));
            let ctx = crate::tool::ToolCtx::new()
                .with_context(context.clone())
                .with_events(sink.clone());
            let message: Message = serde_json::from_value(serde_json::json!({
                "role": "user", "turn_id": TurnId::now(), "parts": [
                    {"type": "text", "text": "inspect"},
                    {"type": "image", "source": {"media_type": "image/png", "data": {"kind": "base64", "data": "iVBORw0KGgo="}}}
                ]
            })).unwrap();
            if detached {
                crate::tools::session::append_message_to_context(&ctx, message).unwrap();
            } else {
                session.append_message(message, None);
            }
            let owner = if detached {
                &context
            } else {
                &session.context()
            };
            let original = owner.messages_handle().lock().unwrap().clone();
            assert!(matches!(
                original[0].parts[1],
                MessagePart::Image { id: Some(_), .. }
            ));
            let mut replacement = original.clone();
            replacement[0].parts.remove(0);
            if detached {
                let mut messages = owner.messages_handle().lock().unwrap();
                sink.emit(Event::Checkpoint {
                    session_id: session.id().to_string(),
                    flow_run_id: None,
                    messages: replacement.clone(),
                    window_tokens: 1,
                });
                *messages = replacement.clone();
            } else {
                assert!(
                    session
                        .commit_rewritten_window(
                            &session.context(),
                            replacement.clone(),
                            100_000,
                            &original,
                            1
                        )
                        .is_some()
                );
            }
            let events = session.sink().snapshot_envelopes();
            let base = if detached {
                ContextBase::Context {
                    context_id: context_id.clone(),
                    through_seq: sink.published_seq(),
                }
            } else {
                ContextBase::LegacyRoot {
                    through_seq: sink.published_seq(),
                }
            };
            let replay = crate::projection::context::replay_context(&events, &base).unwrap();
            assert_eq!(replay.window()[0].1, replacement[0]);
            assert_eq!(replay.raw[0].1, original[0]);
            assert_eq!(*owner.messages_handle().lock().unwrap(), replacement);
            if !detached {
                assert_eq!(session.messages().as_ref(), replacement.as_slice());
                assert_eq!(*session.messages_full(), original);
                let jsonl = events
                    .iter()
                    .map(|event| serde_json::to_string(event).unwrap())
                    .collect::<Vec<_>>()
                    .join("\n");
                let replay = crate::event_log::replay::SessionReplay::from_reader(
                    std::io::Cursor::new(jsonl),
                    None,
                )
                .unwrap();
                assert_eq!(replay.view.compacted[0].1, replacement[0]);
                assert_eq!(replay.view.raw[0].1, original[0]);
            }
        }
    }

    #[test]
    fn message_writers_acquire_the_context_before_publishing() {
        for writer_kind in [
            "root",
            "tool",
            "record",
            "root-record",
            "injection",
            "attachment",
            "fork",
        ] {
            let session = Arc::new(crate::session::Session::open_ephemeral());
            let context = if matches!(writer_kind, "root" | "root-record" | "fork") {
                session.context()
            } else {
                Arc::new(ContextState::new(Vec::new(), Some(session.sink().clone())))
            };
            let sink = session.sink().clone();
            let turn = crate::event::TurnId::now();
            let ctx = crate::tool::ToolCtx::new()
                .with_context(context.clone())
                .with_events(sink.clone())
                .with_anchors(Some(turn.clone()), None, None);
            let ctx = if writer_kind == "root-record" {
                ctx.with_session_runtime(session.clone())
            } else {
                ctx
            };
            let image_id = crate::message::MessagePartId(uuid::Uuid::now_v7());
            if writer_kind == "attachment" {
                let mut message = Message::user_text(turn.clone(), "inspect");
                message.parts.push(crate::message::MessagePart::Image {
                    id: Some(image_id),
                    source: crate::message::ImageSource {
                        media_type: "image/png".into(),
                        data: crate::message::ImageData::Base64 {
                            data: "AA==".into(),
                        },
                        detail: Default::default(),
                    },
                });
                crate::tools::session::append_message_to_context(&ctx, message).unwrap();
            }
            let queue = crate::injection::InjectionQueue::new(Some(sink.clone()));
            queue.enqueue(crate::injection::Injection::new_pending(
                turn.clone(),
                "steering",
            ));
            let claim = queue.claim_steering(|_| true).unwrap();
            let batch = sink.batch();
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| match writer_kind {
                    "fork" => {
                        context
                            .fork(crate::event::ContextInheritance::Full)
                            .unwrap();
                    }
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
                    "record" | "root-record" => {
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
                        claim.commit(Some(&context), || {}).unwrap();
                    }
                    "attachment" => {
                        assert!(
                            context
                                .degrade_attachment(image_id, "invalid_image", Some(turn), None)
                                .is_some()
                        );
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
            let published: Vec<_> = crate::projection::context::replay_context(
                &sink.snapshot_envelopes(),
                &crate::event::ContextBase::LegacyRoot {
                    through_seq: sink.published_seq(),
                },
            )
            .unwrap()
            .raw
            .into_iter()
            .map(|(_, message)| message)
            .collect();
            assert_eq!(context.messages().to_vec(), published, "{writer_kind}");
            assert_eq!(published.len(), usize::from(writer_kind != "fork"));
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
        let child_state = Arc::new(ContextState::new(Vec::new(), None));
        let child = root.clone().with_context(child_state.clone());
        let inline = child.clone();
        assert!(Arc::ptr_eq(root.context().unwrap(), &session.context()));
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
                    true,
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
                    true,
                    prefix.clone()
                )
                .reset_reason,
            None
        );
        assert_eq!(
            root.context()
                .unwrap()
                .observe_prefix("provider", "model", purpose, identity, true, prefix)
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
    fn execution_paths_preserve_the_bound_context() {
        use crate::event::{ContextId, ContextInheritance, Event, EventSink, FlowRunId, TurnId};
        use crate::provider::{AssistantMessage, LlmRequest, Provider};
        use crate::tool::{BoxFut, ContextOwner, ToolCtx};
        use crate::value::Value;

        struct ProbeProvider {
            inner: crate::providers::mock::MockProvider,
            requests: Arc<Mutex<Vec<LlmRequest>>>,
        }

        impl Provider for ProbeProvider {
            fn name(&self) -> &str {
                self.inner.name()
            }

            fn call<'a>(
                &'a self,
                request: LlmRequest,
            ) -> BoxFut<'a, Result<AssistantMessage, crate::RuntimeError>> {
                self.requests.lock().unwrap().push(request.clone());
                self.inner.call(request)
            }

            fn call_streaming(&self, request: LlmRequest) -> crate::Observable<AssistantMessage> {
                self.requests.lock().unwrap().push(request.clone());
                self.inner.call_streaming(request)
            }
        }

        let _registry = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        crate::model_registry::register_model_entries(vec![(
            "context-owner-probe".into(),
            crate::model_registry::ModelEntry {
                model: "context-owner-probe".into(),
                context_budget: Some(100_000),
                ..Default::default()
            },
        )]);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
        for detached in [false, true] {
            for watched in [false, true] {
                for inline in [false, true] {
                  for diagnostics in ["absent", "shared", "independent"] {
                    let session = Arc::new(crate::Session::open_ephemeral());
                    let turn = TurnId::now();
                    session.begin_turn(Message::user_text(turn.clone(), "unselected head"));
                    let context_id = ContextId::now();
                    let sink = session.sink().clone().with_context(context_id.clone());
                    sink.emit(Event::ContextCreated {
                        base: None,
                        inheritance: ContextInheritance::Full,
                    });
                    let initial = Message::user_text(
                        turn.clone(),
                        "selected history",
                    );
                    sink.emit(Event::UserMsg {
                        turn_id: turn.clone(),
                        flow_run_id: None,
                        message: initial.clone(),
                    });
                    let selected = Arc::new(ContextState::new(vec![initial], Some(sink)));
                    let trace = EventSink::new();
                    let run_id = FlowRunId::now();
                    let identity = session
                        .flow_registry
                        .register_root(
                            session.id().to_string(),
                            run_id.clone(),
                            crate::flow_authority::EffectiveAuthority::root(
                                &Default::default(),
                                false,
                                None,
                            ),
                        )
                        .unwrap();
                    let mut tool_ctx = ToolCtx::new()
                        .with_session_runtime(session.clone())
                        .with_permission_broker(session.permission_broker())
                        .with_trust(session.trust_config());
                    if detached {
                        tool_ctx = tool_ctx.with_context(selected.clone());
                        tool_ctx.history_segment = crate::tool::HistorySegment::Spawned;
                    } else {
                        // The selected invocation context need not be the session's default.
                        let Some(ContextOwner::Session { context, .. }) =
                            &mut tool_ctx.context_owner
                        else {
                            unreachable!();
                        };
                        *context = selected.clone();
                    }
                    tool_ctx.flow_identity = Some(identity);
                    tool_ctx.flow_run_id = Some(run_id.clone());
                    tool_ctx.events = match diagnostics {
                        "absent" => None,
                        "shared" => Some(session.sink().clone()),
                        _ => Some(trace.clone()),
                    };
                    let tools = crate::tool::ToolRegistry::new();
                    crate::tools::register_tier_zero(&tools);
                    tools.register(Arc::new(crate::tools::memory::MemoryRecentTurns));
                    let providers = crate::provider::ProviderRegistry::new();
                    let requests = Arc::new(Mutex::new(Vec::new()));
                    providers.register(Arc::new(ProbeProvider {
                        inner: crate::providers::mock::MockProvider::new("context-owner-probe")
                            .with_fallback(Value::Str("bound reply".into())),
                        requests: requests.clone(),
                    }));
                    let watch = if watched {
                        "watch reply { on token(match: \"forbidden-marker\") { abort(\"unexpected token\") } }"
                    } else {
                        ""
                    };
                    let body = format!(
                        "session.push(message.user(\"new input\"))\n reply = llm.call(model: \"context-owner-probe\", context: \"session\")\n {watch}\n session.push(message.user(\"later input\"))\n return memory.recent_turns(n: 10)"
                    );
                    let source = if inline {
                        format!(
                            "flow main() {{ return subflow(helper) }}\n flow helper() {{ {body} }}"
                        )
                    } else {
                        format!("flow main() {{ {body} }}")
                    };
                    let file = atman_dsl::parse::parse_file(&source).unwrap();
                    let flows = file
                        .flows
                        .iter()
                        .map(|flow| (flow.name.name.clone(), flow.clone()))
                        .collect();
                    let result = crate::exec::exec_flow_with_siblings(
                        &file.flows[0],
                        Vec::new(),
                        &tools,
                        &tool_ctx,
                        &providers,
                        &flows,
                        None,
                        Some(turn.clone()),
                        Some(run_id),
                        tokio_util::sync::CancellationToken::new(),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                    assert!(
                        !result.is_err(),
                        "{detached}/{watched}/{inline}: {result:?}"
                    );
                    let requests = requests.lock().unwrap();
                    assert_eq!(requests.len(), 1);
                    let requested: Vec<_> = requests[0]
                        .messages
                        .iter()
                        .map(Message::text_concat)
                        .collect();
                    assert!(requested.iter().any(|text| text == "selected history"));
                    assert!(requested.iter().any(|text| text == "new input"));
                    assert!(!requested.iter().any(|text| text == "unselected head"));
                    let messages = selected.messages();
                    let texts: Vec<_> = messages
                        .iter()
                        .filter(|message| message.origin != crate::message::MessageOrigin::Internal)
                        .map(Message::text_concat)
                        .collect();
                    assert_eq!(
                        texts,
                        [
                            "selected history",
                            "new input",
                            "bound reply",
                            "later input"
                        ]
                    );
                    assert_eq!(session.messages_handle().lock().unwrap().len(), 1);
                    assert_eq!(session.messages().len(), 1);
                    let events = session.sink().snapshot_envelopes();
                    let calls: Vec<_> = events.iter().filter(|envelope| {
                        matches!(envelope.event, Event::LlmCall { .. })
                    }).collect();
                    assert_eq!(calls.len(), 1, "{diagnostics}/{watched}/{inline}");
                    assert_eq!(calls[0].context_id, Some(context_id));
                    let replay = crate::event_log::reader::context_snapshot_from_envelopes(&events).unwrap();
                    assert!(replay.model.is_empty());
                    assert!(replay.provider.is_empty());
                    let traced_calls: Vec<_> = trace.snapshot().into_iter().filter(|event| {
                        matches!(event, Event::LlmCall { .. })
                    }).collect();
                    assert_eq!(traced_calls.len(), usize::from(diagnostics == "independent"));
                    if let Some(traced) = traced_calls.first() {
                        assert_eq!(serde_json::to_value(traced).unwrap(),
                            serde_json::to_value(&calls[0].event).unwrap());
                    }
                    assert!(
                        selected.compaction.model_window_tokens.load(
                            std::sync::atomic::Ordering::Relaxed,
                        ) > 0,
                    );
                    assert_eq!(session.last_input_tokens(), 0);
                    assert_eq!(
                        selected.compaction.last_context_usage.lock().unwrap().entries.len(),
                        1,
                    );
                    assert!(
                        session.context().compaction.last_context_usage.lock().unwrap().entries.is_empty(),
                    );
                    let Value::Struct(fields) = result else {
                        panic!("expected recent-turn result");
                    };
                    let items = fields.iter().find(|(key, _)| key == "items").unwrap();
                    let Value::List(items) = &items.1 else {
                        panic!("expected messages");
                    };
                    let recent: Vec<_> = items
                        .iter()
                        .map(|item| {
                            let Value::Message(message) = item else {
                                panic!("expected message");
                            };
                            message.text_concat()
                        })
                        .collect();
                    assert_eq!(
                        recent,
                        messages
                            .iter()
                            .map(Message::text_concat)
                            .collect::<Vec<_>>()
                    );
                    session.end_turn(&turn);
                  }
                }
            }
        }
            });
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
