//! Ephemeral output of active model calls. No preview is a message publication.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use atman_rt::{TurnLease, TurnOutcome};
use tokio::sync::watch;
use uuid::Uuid;

use super::FlowWorker;
use crate::{
    event::{AtmanUuid, FlowRunId, NodeEvent},
    event_log::journal::CommitReceipt,
    flow_store::FlowRef,
};

#[derive(Clone, Debug)]
pub enum DraftPart {
    Text(String),
    Thinking(String),
    Tool {
        index: usize,
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Clone, Debug)]
pub struct CallDraft {
    pub id: Uuid,
    pub lease: TurnLease<AtmanUuid>,
    pub run: FlowRunId,
    pub model: String,
    pub publication_pending: bool,
    pub attempt: u64,
    pub revision: u64,
    pub parts: Vec<DraftPart>,
}

#[derive(Clone, Debug)]
pub struct LiveOutputSnapshot {
    pub flow: FlowRef,
    /// Durable boundary observed after the preview. Display history must reach it first.
    pub after: CommitReceipt,
    pub revision: u64,
    pub preview_lease: Option<TurnLease<AtmanUuid>>,
    pub calls: Vec<CallDraft>,
}

/// Exact committed completion identity without the retained invocation result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewCompletion {
    pub lease: TurnLease<AtmanUuid>,
    pub outcome: TurnOutcome,
}

/// Bounded output and lifecycle observation independent of transcript preparation.
#[derive(Clone, Debug)]
pub struct PreviewSnapshot {
    pub flow: FlowRef,
    pub text: String,
    pub active: Option<TurnLease<AtmanUuid>>,
    pub completion: Option<PreviewCompletion>,
}

#[derive(Default)]
struct State {
    revision: u64,
    calls: BTreeMap<Uuid, CallDraft>,
    preview: String,
    preview_lease: Option<TurnLease<AtmanUuid>>,
    publications: usize,
    completion: Option<PreviewCompletion>,
}

pub(super) struct LiveOutput {
    flow: FlowRef,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl LiveOutput {
    pub(super) fn sync_store(&self, store: &crate::flow_store::FlowStore) {
        let lease = match store.state().state() {
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) => {
                Some(active.lease.clone())
            }
            _ => None,
        };
        let completion = store.completions().last().map(|record| PreviewCompletion {
            lease: record.completion.lease.clone(),
            outcome: record.completion.outcome,
        });
        let publications = store.published_messages().len();
        self.update(|state| {
            if state.preview_lease == lease
                && state.completion == completion
                && state.publications == publications
            {
                return false;
            }
            if state.publications != publications
                && let Some(preview) = store.published_preview()
            {
                state.preview = preview.to_owned();
            }
            state.publications = publications;
            state.completion = completion;
            state.preview_lease = lease;
            true
        });
    }

    pub fn new(flow: FlowRef) -> Self {
        Self {
            flow,
            state: Mutex::new(State::default()),
            changed: watch::channel(0).0,
        }
    }

    fn update(&self, update: impl FnOnce(&mut State) -> bool) {
        let mut state = self.state.lock().unwrap();
        if update(&mut state) {
            state.revision = state.revision.wrapping_add(1);
            self.changed.send_replace(state.revision);
        }
    }
}

/// A view can disconnect or skip notifications and still read every currently active call.
pub struct LiveOutputReader {
    source: Arc<LiveOutput>,
    commits: watch::Receiver<CommitReceipt>,
    changed: watch::Receiver<u64>,
    seen: u64,
}

impl LiveOutputReader {
    /// Independent observation cursor; consuming this reader never consumes its sibling's updates.
    pub fn subscribe(&self) -> Self {
        Self {
            source: self.source.clone(),
            commits: self.commits.clone(),
            changed: self.source.changed.subscribe(),
            seen: 0,
        }
    }

    pub fn preview_snapshot(&mut self) -> PreviewSnapshot {
        let state = self.source.state.lock().unwrap();
        self.changed.borrow_and_update();
        self.seen = state.revision;
        PreviewSnapshot {
            flow: self.source.flow.clone(),
            text: state.preview.clone(),
            active: state.preview_lease.clone(),
            completion: state.completion.clone(),
        }
    }

    /// Bounded tail for compact activity surfaces; never clone accumulated call bodies.
    pub fn preview(&mut self) -> String {
        let state = self.source.state.lock().unwrap();
        self.changed.borrow_and_update();
        self.seen = state.revision;
        state.preview.clone()
    }

    pub fn snapshot(&mut self) -> LiveOutputSnapshot {
        let state = self.source.state.lock().unwrap();
        self.changed.borrow_and_update();
        self.seen = state.revision;
        LiveOutputSnapshot {
            flow: self.source.flow.clone(),
            after: self.commits.borrow().clone(),
            revision: state.revision,
            preview_lease: state.preview_lease.clone(),
            calls: state.calls.values().cloned().collect(),
        }
    }

    /// Cancellation does not consume a notification. Read the snapshot after waking.
    pub async fn changed(&mut self) {
        let _ = self
            .changed
            .wait_for(|revision| *revision != self.seen)
            .await;
    }
}

#[derive(Clone)]
pub(crate) struct LiveCall {
    inner: Arc<LiveCallInner>,
}

struct LiveCallInner {
    source: Arc<LiveOutput>,
    id: Uuid,
}

impl LiveCall {
    pub fn mark_publication_pending(&self) {
        self.inner.source.update(|state| {
            let call = state.calls.get_mut(&self.inner.id).unwrap();
            if call.publication_pending {
                return false;
            }
            call.publication_pending = true;
            call.revision = call.revision.wrapping_add(1);
            true
        });
    }

    pub fn restart(&self) {
        self.inner.source.update(|state| {
            let call = state.calls.get_mut(&self.inner.id).unwrap();
            call.attempt = call.attempt.wrapping_add(1);
            call.revision = call.revision.wrapping_add(1);
            call.parts.clear();
            true
        });
    }

    pub fn observe(&self, event: &NodeEvent) {
        self.inner.source.update(|state| {
            let call = state.calls.get_mut(&self.inner.id).unwrap();
            match event {
                NodeEvent::LlmChunk { text, .. } | NodeEvent::ThinkingChunk { text } if !text.is_empty() => {
                    let thinking = matches!(event, NodeEvent::ThinkingChunk { .. });
                    match (call.parts.last_mut(), thinking) {
                        (Some(DraftPart::Thinking(current)), true) | (Some(DraftPart::Text(current)), false) => current.push_str(text),
                        (_, true) => call.parts.push(DraftPart::Thinking(text.clone())),
                        (_, false) => call.parts.push(DraftPart::Text(text.clone())),
                    }
                }
                NodeEvent::ToolCallDraft { index, call_id, name, arguments_delta } => {
                    if let Some(DraftPart::Tool { call_id: id, name: tool, arguments, .. }) = call.parts.iter_mut().find(|part| matches!(part, DraftPart::Tool { index: existing, .. } if existing == index)) {
                        if !call_id.is_empty() { *id = call_id.clone(); }
                        if !name.is_empty() { *tool = name.clone(); }
                        arguments.push_str(arguments_delta);
                    } else {
                        call.parts.push(DraftPart::Tool { index: *index, call_id: call_id.clone(), name: name.clone(), arguments: arguments_delta.clone() });
                    }
                }
                _ => return false,
            }
            call.revision = call.revision.wrapping_add(1);
            let text = match call.parts.last().unwrap() {
                DraftPart::Text(text) | DraftPart::Thinking(text) => text,
                DraftPart::Tool { name, arguments, .. } => {
                    if arguments.is_empty() { name } else { arguments }
                }
            };
            let tail: String = text.chars().rev().take(512).collect();
            state.preview = crate::flow_store::compact_preview(&tail.chars().rev().collect::<String>());
            true
        });
    }
}

impl Drop for LiveCallInner {
    fn drop(&mut self) {
        self.source
            .update(|state| state.calls.remove(&self.id).is_some());
    }
}

impl FlowWorker {
    pub fn subscribe_live_output(&self) -> LiveOutputReader {
        LiveOutputReader {
            changed: self.live_output.changed.subscribe(),
            source: self.live_output.clone(),
            commits: self.commits.clone(),
            seen: 0,
        }
    }

    pub(crate) fn begin_live_call(
        &self,
        lease: TurnLease<AtmanUuid>,
        run: FlowRunId,
        model: String,
    ) -> LiveCall {
        assert_eq!(&lease.flow, self.address());
        let id = Uuid::now_v7();
        self.live_output.update(|state| {
            if state.preview_lease.as_ref() != Some(&lease) {
                state.preview_lease = Some(lease.clone());
            }
            state.calls.insert(
                id,
                CallDraft {
                    id,
                    lease,
                    run,
                    model,
                    publication_pending: false,
                    attempt: 0,
                    revision: 0,
                    parts: Vec::new(),
                },
            );
            true
        });
        LiveCall {
            inner: Arc::new(LiveCallInner {
                source: self.live_output.clone(),
                id,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compact_observers_retain_shared_output_and_exact_completion_across_coalescing_and_reopen()
     {
        use crate::flow_store::{
            EntrySnapshot, FlowDescriptor, FlowTransition, RuntimeStore, SessionDescriptor,
        };
        use crate::flow_store::{
            completion::{InvocationResult, StoredValue},
            publication::MessagePublication,
        };
        use crate::{message::Message, source_program::RevisionBundle};

        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let session_id = Uuid::now_v7();
        let mut session = root
            .create_session(SessionDescriptor {
                id: session_id,
                configuration: Default::default(),
                project: None,
                title: "Compact observation".into(),
            })
            .unwrap();
        let flow = FlowRef {
            session_id: session_id.into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let store = session
            .create_flow(FlowDescriptor {
                address: flow.clone(),
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        let worker = FlowWorker::start(store).await.unwrap();
        worker
            .transition(0, FlowTransition::Activated)
            .await
            .unwrap();
        let mut original = worker.subscribe_live_output();
        let mut compact = original.subscribe();
        assert!(compact.preview_snapshot().completion.is_none());
        let lease = worker
            .state()
            .await
            .unwrap()
            .begin_turn(crate::event::TurnId::now(), FlowRunId::now())
            .unwrap();
        worker
            .transition(
                1,
                FlowTransition::TurnStarted {
                    turn: lease.turn.clone(),
                    run: lease.run.clone(),
                },
            )
            .await
            .unwrap();
        let published = MessagePublication {
            id: Uuid::now_v7(),
            lease: lease.clone(),
            run: lease.run.clone(),
            context_run: None,
            message: Message::assistant_text(lease.turn.clone(), "界".repeat(700)),
        };
        let shared = published.clone();
        worker
            .request(move |store| store.publish_message(shared))
            .await
            .unwrap();
        let mut isolated = published;
        isolated.id = Uuid::now_v7();
        isolated.context_run = Some(lease.run.clone());
        isolated.message = Message::assistant_text(lease.turn.clone(), "Isolated context");
        worker
            .request(move |store| store.publish_message(isolated))
            .await
            .unwrap();
        let finished = lease.clone();
        worker
            .request(move |store| {
                store.finish_turn(finished, InvocationResult::Returned(StoredValue::Unit))
            })
            .await
            .unwrap();
        original.snapshot();
        compact.changed().await;
        let snapshot = compact.preview_snapshot();
        assert_eq!(snapshot.flow, flow);
        assert_eq!(snapshot.text, "界".repeat(512));
        assert!(snapshot.active.is_none());
        assert_eq!(
            snapshot.completion,
            Some(PreviewCompletion {
                lease,
                outcome: TurnOutcome::Completed
            })
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), compact.changed())
                .await
                .is_err()
        );
        drop(worker.shutdown().await.unwrap());
        let reopened = FlowWorker::start(session.open_flow(&flow.instance_id).unwrap())
            .await
            .unwrap();
        let restored = reopened.subscribe_live_output().preview_snapshot();
        assert_eq!(restored.text, snapshot.text);
        assert_eq!(restored.completion, snapshot.completion);
        drop(reopened.shutdown().await.unwrap());
    }

    #[tokio::test]
    async fn live_output_consumers_and_call_guards_have_independent_lifetimes() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let source = Arc::new(LiveOutput::new(flow.clone()));
        let lease = TurnLease {
            flow,
            epoch: 1,
            turn: crate::event::TurnId::now(),
            run: FlowRunId::now(),
        };
        let create = || {
            let id = Uuid::now_v7();
            source.update(|state| {
                state.calls.insert(
                    id,
                    CallDraft {
                        id,
                        lease: lease.clone(),
                        run: lease.run.clone(),
                        model: "example".into(),
                        publication_pending: false,
                        attempt: 0,
                        revision: 0,
                        parts: Vec::new(),
                    },
                );
                true
            });
            LiveCall {
                inner: Arc::new(LiveCallInner {
                    source: source.clone(),
                    id,
                }),
            }
        };
        let mut first = LiveOutputReader {
            source: source.clone(),
            commits: watch::channel(CommitReceipt {
                journal_id: Uuid::now_v7(),
                sequence: 1,
                digest: "test-boundary".into(),
            })
            .1,
            changed: source.changed.subscribe(),
            seen: 0,
        };
        first.snapshot();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), first.changed())
                .await
                .is_err()
        );
        let one = create();
        let two = create();
        let one_guard = one.clone();
        one.observe(&NodeEvent::LlmChunk {
            text: "first".into(),
            cumulative_tokens: 1,
        });
        two.observe(&NodeEvent::ThinkingChunk {
            text: "second".into(),
        });
        first.changed().await;
        // Observing readiness alone must not lose the pending snapshot.
        first.changed().await;
        let mut second = LiveOutputReader {
            source: source.clone(),
            commits: first.commits.clone(),
            changed: source.changed.subscribe(),
            seen: 0,
        };
        assert_eq!(first.snapshot().calls.len(), 2);
        assert_eq!(second.snapshot().calls.len(), 2);
        assert_eq!(second.preview(), "second");
        two.observe(&NodeEvent::ThinkingChunk {
            text: "界".repeat(600),
        });
        second.changed().await;
        let tail = second.preview();
        assert_eq!(tail.chars().count(), 512);
        assert!(tail.ends_with('界'));
        one.restart();
        one.observe(&NodeEvent::LlmChunk {
            text: "replacement".into(),
            cumulative_tokens: 1,
        });
        let snapshot = first.snapshot();
        assert_eq!(snapshot.calls[0].attempt, 1);
        assert!(
            matches!(&snapshot.calls[0].parts[..], [DraftPart::Text(text)] if text == "replacement")
        );
        assert!(
            matches!(&snapshot.calls[1].parts[..], [DraftPart::Thinking(text)] if text.starts_with("second") && text.ends_with('界'))
        );
        one.mark_publication_pending();
        assert!(first.snapshot().calls[0].publication_pending);
        drop(one);
        assert_eq!(first.snapshot().calls.len(), 2);
        drop(one_guard);
        assert_eq!(first.snapshot().calls.len(), 1);
        drop(first);
        second.changed().await;
        assert_eq!(second.snapshot().calls.len(), 1);
        drop(two);
        second.changed().await;
        assert!(second.snapshot().calls.is_empty());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), second.changed())
                .await
                .is_err()
        );
    }
}
