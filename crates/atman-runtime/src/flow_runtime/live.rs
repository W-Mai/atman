//! Ephemeral output of active model calls. No preview is a message publication.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use atman_rt::TurnLease;
use tokio::sync::watch;
use uuid::Uuid;

use super::FlowWorker;
use crate::{
    event::{AtmanUuid, FlowRunId, NodeEvent},
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
    pub attempt: u64,
    pub revision: u64,
    pub parts: Vec<DraftPart>,
}

#[derive(Clone, Debug)]
pub struct LiveOutputSnapshot {
    pub flow: FlowRef,
    pub revision: u64,
    pub calls: Vec<CallDraft>,
}

#[derive(Default)]
struct State {
    revision: u64,
    calls: BTreeMap<Uuid, CallDraft>,
    preview: String,
    preview_lease: Option<TurnLease<AtmanUuid>>,
}

pub(super) struct LiveOutput {
    flow: FlowRef,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl LiveOutput {
    pub(super) fn sync_execution(&self, execution: &atman_rt::FlowInstance<AtmanUuid>) {
        let lease = match execution.state() {
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) => {
                Some(active.lease.clone())
            }
            _ => None,
        };
        self.update(|state| {
            if state.preview_lease == lease {
                return false;
            }
            state.preview_lease = lease;
            false
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
    changed: watch::Receiver<u64>,
    seen: u64,
}

impl LiveOutputReader {
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
            revision: state.revision,
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

pub(crate) struct LiveCall {
    source: Arc<LiveOutput>,
    id: Uuid,
}

impl LiveCall {
    pub fn restart(&self) {
        self.source.update(|state| {
            let call = state.calls.get_mut(&self.id).unwrap();
            call.attempt = call.attempt.wrapping_add(1);
            call.revision = call.revision.wrapping_add(1);
            call.parts.clear();
            true
        });
    }

    pub fn observe(&self, event: &NodeEvent) {
        self.source.update(|state| {
            let call = state.calls.get_mut(&self.id).unwrap();
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

impl Drop for LiveCall {
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
                    attempt: 0,
                    revision: 0,
                    parts: Vec::new(),
                },
            );
            true
        });
        LiveCall {
            source: self.live_output.clone(),
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                        attempt: 0,
                        revision: 0,
                        parts: Vec::new(),
                    },
                );
                true
            });
            LiveCall {
                source: source.clone(),
                id,
            }
        };
        let mut first = LiveOutputReader {
            source: source.clone(),
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
        drop(first);
        drop(one);
        second.changed().await;
        assert_eq!(second.snapshot().calls[0].id, two.id);
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
