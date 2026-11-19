//! Bounded browser observation of the original live producer and its acknowledged response.

use std::{collections::VecDeque, sync::Arc};

use atman_rt::TurnLease;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{CallDraft, DraftPart, FlowWorker, LiveCall, LiveOutput, State};
use crate::{
    event::{AtmanUuid, FlowRunId},
    flow_store::{
        FlowRef, FlowStore,
        branches::HistoryHead,
        position::FlowPosition,
        publication::{MessagePublication, PublishedMessage},
    },
    message::{MessageOrigin, MessagePart, MessageRole},
    projection::flow::FlowRecordId,
    tools::final_answer::FINAL_ANSWER_TOOL,
};

const MAX_CALLS: usize = 8;
const MAX_PARTS: usize = 48;
const MAX_PART_BYTES: usize = 16 * 1024;
const MAX_TOOL_LABEL_BYTES: usize = 256;
const MAX_CONTENT_BYTES: usize = 64 * 1024;
const MAX_HANDOFFS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftObservationSource {
    pub flow: FlowRef,
    pub head: HistoryHead,
    pub host_generation: Uuid,
    pub worker_incarnation: Uuid,
}

#[derive(Clone, Debug)]
pub struct DraftObservation {
    pub source: DraftObservationSource,
    /// Install this complete durable prefix before projecting a draft or its handoff.
    pub after: FlowPosition,
    pub revision: u64,
    pub active: Option<TurnLease<AtmanUuid>>,
    pub work: Option<DraftWorkScope>,
    pub calls: Vec<ObservedDraftCall>,
    pub final_answer: Option<ObservedDraftCall>,
    pub handoffs: Vec<DraftHandoff>,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct DraftWorkScope {
    pub lease: TurnLease<AtmanUuid>,
    pub admission: Option<FlowPosition>,
    pub latest_final: Option<FinalWorkBoundary>,
}

#[derive(Clone, Debug)]
pub struct FinalWorkBoundary {
    pub publication: Uuid,
    pub receipt: FlowPosition,
}

pub(super) fn work_scope(store: &FlowStore, lease: &TurnLease<AtmanUuid>) -> DraftWorkScope {
    DraftWorkScope {
        lease: lease.clone(),
        admission: store
            .last_invocation_ref()
            .filter(|admission| &admission.lease == lease)
            .map(|admission| admission.receipt.clone()),
        latest_final: store
            .latest_shared_final()
            .filter(|published| &published.lease == lease)
            .map(|published| FinalWorkBoundary {
                publication: published.id,
                receipt: published.receipt.clone(),
            }),
    }
}

#[derive(Clone, Debug)]
pub struct ObservedDraftCall {
    pub id: Uuid,
    pub lease: TurnLease<AtmanUuid>,
    pub run: FlowRunId,
    pub attempt: u64,
    pub revision: u64,
    pub publication_pending: bool,
    pub parts: Vec<ObservedDraftPart>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftDisplayId {
    pub call: Uuid,
    pub attempt: u64,
    pub part: usize,
}

#[derive(Clone, Debug)]
pub struct ObservedDraftPart {
    pub id: DraftDisplayId,
    pub body: ObservedDraftPartBody,
}

#[derive(Clone, Debug)]
pub enum ObservedDraftPartBody {
    Thinking(BoundedDraftText),
    Text(BoundedDraftText),
    FinalAnswer(BoundedDraftText),
    Tool {
        index: usize,
        call_id: BoundedDraftText,
        name: BoundedDraftText,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedDraftText {
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct DraftHandoff {
    pub lease: TurnLease<AtmanUuid>,
    pub run: FlowRunId,
    pub publication: Uuid,
    pub receipt: FlowPosition,
    pub record: FlowRecordId,
    pub parts: Vec<DraftHandoffPart>,
}

/// A whole directly matched block. Ordinary tools are one contiguous ordered group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftHandoffPart {
    pub live: DraftDisplayId,
    pub published_parts: Vec<usize>,
}

#[derive(Default)]
pub(super) struct ObservationState {
    after: Option<FlowPosition>,
    revision: u64,
    handoffs: VecDeque<(HistoryHead, DraftHandoff)>,
    pub(super) work: Option<DraftWorkScope>,
    final_answer: Option<ObservedDraftCall>,
    final_truncated: bool,
}

impl ObservationState {
    pub(super) fn advance(&mut self, after: &FlowPosition) -> bool {
        if self.after.as_ref() == Some(after) {
            return false;
        }
        self.after = Some(after.clone());
        true
    }

    pub(super) fn tick(&mut self) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        self.revision
    }

    fn remember(&mut self, head: HistoryHead, witness: DraftHandoff) {
        if self.handoffs.len() == MAX_HANDOFFS {
            self.handoffs.pop_front();
        }
        self.handoffs.push_back((head, witness));
    }
}

/// Does not hold the journal writer, execution permit, host directory, or an admission token.
pub struct DraftObservationReader {
    source: Arc<LiveOutput>,
    host_generation: Uuid,
    lifetime: CancellationToken,
    changed: watch::Receiver<u64>,
}

impl FlowWorker {
    /// The caller must authorize the Flow first and resolve an already loaded owner.
    pub fn observe_workbench_drafts(&self) -> Option<DraftObservationReader> {
        let host = self.host.as_ref()?.upgrade().ok()?;
        if self.is_stopped() || self.is_transient() || self.resources.lifetime.is_cancelled() {
            return None;
        }
        Some(DraftObservationReader::new(self, host.generation()))
    }
}

pub(in crate::flow_runtime) struct PreparedFinalDraft {
    source: Arc<LiveOutput>,
    id: Uuid,
    head: HistoryHead,
}

impl LiveOutput {
    /// Called only by the normalized final-answer publication path.
    pub(in crate::flow_runtime) fn prepare_final(
        self: &Arc<Self>,
        publication: &MessagePublication,
    ) -> Option<PreparedFinalDraft> {
        let mut state = self.state.lock().unwrap();
        let head = state.history_head.clone()?;
        if state.preview_lease.as_ref() != Some(&publication.lease)
            || state.workbench.final_answer.is_some()
            || publication.context_run.is_some()
            || publication.message.role != MessageRole::Assistant
            || publication.message.origin != MessageOrigin::FinalAnswer
        {
            return None;
        }
        let mut remaining = MAX_CONTENT_BYTES;
        let parts = publication
            .message
            .parts
            .iter()
            .take(MAX_PARTS)
            .enumerate()
            .filter_map(|(part, source)| {
                let body = match source {
                    MessagePart::Thinking { thinking, .. } => ObservedDraftPartBody::Thinking(
                        bounded(thinking, MAX_PART_BYTES, &mut remaining),
                    ),
                    MessagePart::Text { text } => ObservedDraftPartBody::FinalAnswer(bounded(
                        text,
                        MAX_PART_BYTES,
                        &mut remaining,
                    )),
                    _ => return None,
                };
                Some(ObservedDraftPart {
                    id: DraftDisplayId {
                        call: publication.id,
                        attempt: 0,
                        part,
                    },
                    body,
                })
            })
            .collect::<Vec<_>>();
        if !parts
            .iter()
            .any(|part| matches!(&part.body, ObservedDraftPartBody::FinalAnswer(_)))
        {
            return None;
        }
        state.workbench.final_truncated = publication.message.parts.len() > MAX_PARTS
            || parts.iter().any(|part| match &part.body {
                ObservedDraftPartBody::Thinking(value)
                | ObservedDraftPartBody::FinalAnswer(value) => value.truncated,
                _ => false,
            });
        state.workbench.final_answer = Some(ObservedDraftCall {
            id: publication.id,
            lease: publication.lease.clone(),
            run: publication.run.clone(),
            attempt: 0,
            revision: 0,
            publication_pending: true,
            parts,
        });
        self.notify_workbench(&mut state);
        Some(PreparedFinalDraft {
            source: self.clone(),
            id: publication.id,
            head,
        })
    }
}

impl PreparedFinalDraft {
    pub(in crate::flow_runtime) fn witness_publication(&self, saved: &PublishedMessage) {
        let mut state = self.source.state.lock().unwrap();
        let Some(draft) = state
            .workbench
            .final_answer
            .as_ref()
            .filter(|call| call.id == self.id)
        else {
            return;
        };
        if state.history_head.as_ref() != Some(&self.head)
            || saved.publication.id != draft.id
            || saved.publication.lease != draft.lease
            || saved.publication.run != draft.run
            || saved.publication.message.origin != MessageOrigin::FinalAnswer
        {
            return;
        }
        let Some(receipt) = saved.receipt.require_durable().ok() else {
            return;
        };
        let parts = draft
            .parts
            .iter()
            .filter_map(|part| {
                let actual = saved.publication.message.parts.get(part.id.part)?;
                let (preview, actual) = match (&part.body, actual) {
                    (
                        ObservedDraftPartBody::Thinking(value),
                        MessagePart::Thinking { thinking, .. },
                    )
                    | (
                        ObservedDraftPartBody::FinalAnswer(value),
                        MessagePart::Text { text: thinking },
                    ) => (value, thinking),
                    _ => return None,
                };
                (!preview.truncated && &preview.text == actual).then(|| DraftHandoffPart {
                    live: part.id.clone(),
                    published_parts: vec![part.id.part],
                })
            })
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            state.workbench.remember(
                self.head.clone(),
                DraftHandoff {
                    lease: saved.publication.lease.clone(),
                    run: saved.publication.run.clone(),
                    publication: self.id,
                    receipt: saved.receipt.clone(),
                    record: FlowRecordId {
                        commit: receipt.sequence,
                        fact: 0,
                        item: 0,
                    },
                    parts,
                },
            );
        }
    }
}

impl Drop for PreparedFinalDraft {
    fn drop(&mut self) {
        let mut state = self.source.state.lock().unwrap();
        if state
            .workbench
            .final_answer
            .as_ref()
            .is_some_and(|draft| draft.id == self.id)
        {
            state.workbench.final_answer = None;
            state.workbench.final_truncated = false;
            self.source.notify_workbench(&mut state);
        }
    }
}

impl DraftObservationReader {
    fn new(worker: &FlowWorker, host_generation: Uuid) -> Self {
        Self {
            source: worker.live_output.clone(),
            host_generation,
            lifetime: worker.resources.lifetime.clone(),
            changed: worker.live_output.workbench_changed.subscribe(),
        }
    }

    #[cfg(test)]
    pub(in crate::flow_runtime) fn observe_for_test(worker: &FlowWorker) -> Self {
        Self::new(worker, Uuid::now_v7())
    }

    pub fn snapshot(&mut self) -> Option<DraftObservation> {
        let state = self.source.state.lock().unwrap();
        if self.lifetime.is_cancelled() {
            return None;
        }
        self.changed.borrow_and_update();
        let head = state.history_head.clone()?;
        let after = state.workbench.after.as_ref()?;
        after.require_durable().ok()?;
        let final_answer = state
            .workbench
            .final_answer
            .as_ref()
            .filter(|call| state.preview_lease.as_ref() == Some(&call.lease))
            .cloned();
        let (mut remaining_parts, mut remaining_bytes) = (MAX_PARTS, MAX_CONTENT_BYTES);
        if let Some(final_answer) = &final_answer {
            remaining_parts -= final_answer.parts.len();
            for part in &final_answer.parts {
                if let ObservedDraftPartBody::Thinking(value)
                | ObservedDraftPartBody::FinalAnswer(value) = &part.body
                {
                    remaining_bytes -= value.text.len();
                }
            }
        }
        let (calls, mut truncated) = observe_calls(&state, remaining_parts, remaining_bytes);
        truncated |= final_answer.is_some() && state.workbench.final_truncated;
        Some(DraftObservation {
            source: DraftObservationSource {
                flow: self.source.flow.clone(),
                head: head.clone(),
                host_generation: self.host_generation,
                worker_incarnation: self.source.incarnation,
            },
            after: after.clone(),
            revision: state.workbench.revision,
            active: state.preview_lease.clone(),
            work: state.workbench.work.clone(),
            truncated,
            calls,
            final_answer,
            handoffs: state
                .workbench
                .handoffs
                .iter()
                .filter(|(at, handoff)| {
                    at == &head
                        && after.same_history(&handoff.receipt)
                        && after.sequence() >= handoff.receipt.sequence()
                })
                .map(|(_, handoff)| handoff.clone())
                .collect(),
        })
    }

    /// A false return retires this source. A true return requires another snapshot.
    pub async fn changed(&mut self) -> bool {
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => false,
            changed = self.changed.changed() => changed.is_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::TurnId,
        event_log::journal::CommitReceipt,
        flow_store::{FlowInstanceId, publication::MessagePublication},
        message::{Message, MessageOrigin},
    };

    #[test]
    fn bounds_multibyte_text_without_spending_the_remainder() {
        let mut budget = 5;
        let value = bounded("界界", MAX_PART_BYTES, &mut budget);
        assert_eq!(value.text, "界");
        assert!(value.truncated);
        assert_eq!(budget, 2);
        assert_eq!(bounded("ok", MAX_PART_BYTES, &mut budget).text, "ok");
        assert_eq!(budget, 0);
    }

    #[test]
    fn exact_final_publication_retains_its_normalized_parts_after_preview_retirement() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let lease = TurnLease {
            flow: flow.clone(),
            epoch: 1,
            turn: TurnId::now(),
            run: FlowRunId::now(),
        };
        let head = HistoryHead::initial(&flow, flow.instance_id.0.0);
        let source = Arc::new(LiveOutput::new(flow.clone()));
        let receipt = |sequence| {
            FlowPosition::Durable(CommitReceipt {
                journal_id: flow.instance_id.0.0,
                sequence,
                digest: format!("{sequence:064x}"),
            })
        };
        source.update_at(Some(&receipt(1)), |state| {
            state.preview_lease = Some(lease.clone());
            state.history_head = Some(head);
            true
        });
        let mut message = Message::assistant_text(lease.turn.clone(), "Final answer");
        message.origin = MessageOrigin::FinalAnswer;
        let publication = MessagePublication {
            id: Uuid::now_v7(),
            run: lease.run.clone(),
            lease,
            context_run: None,
            message,
        };
        let mut reader = DraftObservationReader {
            changed: source.workbench_changed.subscribe(),
            source: source.clone(),
            host_generation: Uuid::now_v7(),
            lifetime: CancellationToken::new(),
        };
        let prepared = source.prepare_final(&publication).unwrap();
        let pending = reader.snapshot().unwrap();
        let final_answer = pending.final_answer.unwrap();
        let identity = final_answer.parts[0].id.clone();
        assert!(
            matches!(&final_answer.parts[0].body, ObservedDraftPartBody::FinalAnswer(value) if value.text == "Final answer")
        );
        assert!(pending.handoffs.is_empty());
        let mut other = publication.clone();
        other.id = Uuid::now_v7();
        prepared.witness_publication(&PublishedMessage {
            publication: other,
            receipt: receipt(2),
        });
        assert!(source.state.lock().unwrap().workbench.handoffs.is_empty());
        prepared.witness_publication(&PublishedMessage {
            publication,
            receipt: receipt(2),
        });
        assert!(reader.snapshot().unwrap().handoffs.is_empty());
        source.update_at(Some(&receipt(2)), |_| false);
        drop(prepared);
        let committed = reader.snapshot().unwrap();
        assert!(committed.final_answer.is_none());
        assert_eq!(committed.handoffs[0].receipt, receipt(2));
        assert_eq!(committed.handoffs[0].parts[0].live, identity);
    }

    #[test]
    fn matches_only_exact_ordered_tools_of_the_bound_call() {
        let run = FlowRunId::now();
        let lease = TurnLease {
            flow: FlowRef {
                session_id: Uuid::now_v7().into(),
                instance_id: FlowInstanceId::now(),
            },
            epoch: 1,
            turn: TurnId::now(),
            run: run.clone(),
        };
        let call = CallDraft {
            id: Uuid::now_v7(),
            lease: lease.clone(),
            run: run.clone(),
            model: "example".into(),
            publication_pending: true,
            attempt: 2,
            revision: 3,
            parts: vec![
                DraftPart::Thinking("ready".into()),
                DraftPart::Tool {
                    index: 0,
                    call_id: "one".into(),
                    name: "first".into(),
                    arguments: "unprojected".into(),
                },
                DraftPart::Tool {
                    index: 1,
                    call_id: "two".into(),
                    name: "second".into(),
                    arguments: "unprojected".into(),
                },
            ],
        };
        let actual_tool = |id: &str, name: &str| MessagePart::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::Value::Null,
            intent: None,
        };
        let mut saved = PublishedMessage {
            publication: MessagePublication {
                id: Uuid::now_v7(),
                lease: lease.clone(),
                run,
                context_run: None,
                message: Message {
                    role: MessageRole::Assistant,
                    turn_id: lease.turn,
                    origin: MessageOrigin::User,
                    parts: vec![
                        MessagePart::Thinking {
                            thinking: "ready".into(),
                            signature: None,
                        },
                        actual_tool("one", "first"),
                        actual_tool("two", "second"),
                    ],
                },
            },
            receipt: FlowPosition::Durable(CommitReceipt {
                journal_id: Uuid::now_v7(),
                sequence: 6,
                digest: "test-boundary".into(),
            }),
        };
        let witness = match_publication(&call, &saved).unwrap();
        assert_eq!(witness.parts.len(), 2);
        assert_eq!(witness.parts[1].live.attempt, 2);
        assert_eq!(witness.parts[1].published_parts, vec![1, 2]);
        saved.publication.message.parts.swap(1, 2);
        assert!(match_publication(&call, &saved).is_none());
        saved.publication.message.parts.swap(1, 2);
        saved.publication.context_run = Some(call.run.clone());
        assert!(match_publication(&call, &saved).is_none());
    }
}

fn observe_calls(
    state: &State,
    mut remaining_parts: usize,
    mut remaining_bytes: usize,
) -> (Vec<ObservedDraftCall>, bool) {
    let mut truncated = state.calls.len() > MAX_CALLS;
    let mut calls = Vec::new();
    let Some(lease) = state.preview_lease.as_ref() else {
        return (calls, false);
    };
    for registered in state.calls.values().take(MAX_CALLS) {
        let call = &registered.draft;
        if registered.history_head != state.history_head || &call.lease != lease {
            continue;
        }
        truncated |= call.parts.len() > remaining_parts;
        let parts = call
            .parts
            .iter()
            .take(remaining_parts)
            .enumerate()
            .map(|(part, draft)| {
                let body = match draft {
                    DraftPart::Thinking(text) => ObservedDraftPartBody::Thinking(bounded(
                        text,
                        MAX_PART_BYTES,
                        &mut remaining_bytes,
                    )),
                    DraftPart::Text(text) => ObservedDraftPartBody::Text(bounded(
                        text,
                        MAX_PART_BYTES,
                        &mut remaining_bytes,
                    )),
                    DraftPart::Tool {
                        index,
                        call_id,
                        name,
                        ..
                    } => ObservedDraftPartBody::Tool {
                        index: *index,
                        call_id: bounded(call_id, MAX_TOOL_LABEL_BYTES, &mut remaining_bytes),
                        name: bounded(name, MAX_TOOL_LABEL_BYTES, &mut remaining_bytes),
                    },
                };
                ObservedDraftPart {
                    id: display_id(call, part),
                    body,
                }
            })
            .collect::<Vec<_>>();
        remaining_parts -= parts.len();
        truncated |= parts.iter().any(|part| match &part.body {
            ObservedDraftPartBody::Thinking(text)
            | ObservedDraftPartBody::Text(text)
            | ObservedDraftPartBody::FinalAnswer(text) => text.truncated,
            ObservedDraftPartBody::Tool { call_id, name, .. } => {
                call_id.truncated || name.truncated
            }
        });
        calls.push(ObservedDraftCall {
            id: call.id,
            lease: call.lease.clone(),
            run: call.run.clone(),
            attempt: call.attempt,
            revision: call.revision,
            publication_pending: call.publication_pending,
            parts,
        });
    }
    (calls, truncated)
}

fn bounded(text: &str, limit: usize, remaining: &mut usize) -> BoundedDraftText {
    let mut end = text.len().min(limit).min(*remaining);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *remaining -= end;
    BoundedDraftText {
        text: text[..end].to_owned(),
        truncated: end < text.len(),
    }
}

fn display_id(call: &CallDraft, part: usize) -> DraftDisplayId {
    DraftDisplayId {
        call: call.id,
        attempt: call.attempt,
        part,
    }
}

impl LiveCall {
    pub(in crate::flow_runtime) fn belongs_to(&self, source: &Arc<LiveOutput>) -> bool {
        Arc::ptr_eq(&self.inner.source, source)
    }

    /// The exact producer guard travels in the accepted storage command until acknowledgement.
    pub(in crate::flow_runtime) fn witness_publication(&self, saved: &PublishedMessage) {
        let mut state = self.inner.source.state.lock().unwrap();
        let Some(head) = state.history_head.clone() else {
            return;
        };
        let Some(call) = state.current_call(self.inner.id) else {
            return;
        };
        if let Some(witness) = match_publication(call, saved) {
            state.workbench.remember(head, witness);
        }
    }
}

fn match_publication(call: &CallDraft, saved: &PublishedMessage) -> Option<DraftHandoff> {
    let publication = &saved.publication;
    let receipt = saved.receipt.require_durable().ok()?;
    if publication.lease != call.lease
        || publication.run != call.run
        || publication.context_run.is_some()
        || publication.message.role != MessageRole::Assistant
        || call.parts.len() > MAX_PARTS
        || publication.message.parts.len() > MAX_PARTS
    {
        return None;
    }
    let mut published = publication
        .message
        .parts
        .iter()
        .enumerate()
        .filter(|(_, part)| !matches!(part, MessagePart::ResponsesState { .. }));
    let mut parts: Vec<DraftHandoffPart> = Vec::new();
    let mut in_tools = false;
    for (part, draft) in call.parts.iter().enumerate() {
        let (published_part, item) = published.next()?;
        let tools = match (draft, item) {
            (DraftPart::Text(left), MessagePart::Text { text: right })
            | (
                DraftPart::Thinking(left),
                MessagePart::Thinking {
                    thinking: right, ..
                },
            ) if left.len() <= MAX_PART_BYTES && left == right => false,
            (
                DraftPart::Tool { call_id, name, .. },
                MessagePart::ToolUse { id, name: tool, .. },
            ) if !call_id.is_empty()
                && !name.is_empty()
                && call_id.len() <= MAX_TOOL_LABEL_BYTES
                && name.len() <= MAX_TOOL_LABEL_BYTES
                && call_id == id
                && name == tool =>
            {
                if name == FINAL_ANSWER_TOOL {
                    // A final answer has its own authoritative publication path.
                    in_tools = false;
                    continue;
                }
                true
            }
            _ => return None,
        };
        if tools && in_tools {
            parts.last_mut()?.published_parts.push(published_part);
        } else {
            parts.push(DraftHandoffPart {
                live: display_id(call, part),
                published_parts: vec![published_part],
            });
        }
        in_tools = tools;
    }
    if published.next().is_some() || parts.is_empty() {
        return None;
    }
    Some(DraftHandoff {
        lease: call.lease.clone(),
        run: call.run.clone(),
        publication: publication.id,
        receipt: saved.receipt.clone(),
        // FlowStore::publish_message commits one MessagePublished fact per batch.
        record: FlowRecordId {
            commit: receipt.sequence,
            fact: 0,
            item: 0,
        },
        parts,
    })
}
