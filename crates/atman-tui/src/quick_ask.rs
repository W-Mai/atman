//! Question identity and operation ownership, independent of a displayed source document.

use atman_runtime::flow_store::{FlowRef, source_capture::SourceSelection};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::control::{ControlTicket, ViewAddress, ViewBinding};

pub(crate) mod presentation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Refresh,
    Keep,
    Discard,
}

pub enum Command {
    Open(QuickAsk),
    Operate(
        Operation,
        Option<atman_runtime::provider::ReasoningSelection>,
    ),
    Installed {
        kind: HandoffKind,
        request_id: Uuid,
    },
    Retry(Retry),
    SwitchModel {
        request_id: u64,
        model: String,
        effort: Option<atman_runtime::provider::ReasoningSelection>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retry {
    Handoff { kind: HandoffKind, request_id: Uuid },
    Resume,
}

#[derive(Clone, Debug)]
pub(crate) enum Intent {
    Actions,
    Open {
        selection: Option<String>,
        fresh: bool,
        source: Option<SourceSelection>,
    },
    Action(presentation::Action),
}

pub(crate) struct PendingBring {
    pub id: Uuid,
    pub target: ViewBinding,
    pub source: SourceSelection,
    pub text: String,
}

#[derive(Debug)]
struct State {
    phase: Phase,
    model: String,
    pending: Option<(Operation, ControlTicket)>,
    captured: SourceSelection,
    initial_effort: Option<atman_runtime::provider::ReasoningSelection>,
    excerpt: Option<String>,
    source_label: String,
    source_detail: String,
    captured_at: Option<chrono::DateTime<chrono::Utc>>,
    retention: Option<atman_runtime::flow_store::retention::RetentionReceipt>,
    retention_pending: bool,
    retry: Option<Retry>,
    retrying: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffKind {
    Launch,
    Keep,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Phase {
    Draft,
    Transient(FlowRef),
    Kept(FlowRef),
}

/// The source remains immutable as the question acquires its own execution and durable identity.
#[derive(Clone, Debug)]
pub struct QuickAsk {
    pub id: Uuid,
    pub source: SourceSelection,
    /// A resolved model identifier, never an alias whose global binding can change.
    pub model: String,
    state: Arc<Mutex<State>>,
}

impl QuickAsk {
    pub fn new(source: SourceSelection, model: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            state: Arc::new(Mutex::new(State {
                phase: Phase::Draft,
                model: model.clone(),
                pending: None,
                captured: source.clone(),
                initial_effort: None,
                excerpt: None,
                source_label: "Source".into(),
                source_detail: String::new(),
                captured_at: None,
                retention: None,
                retention_pending: false,
                retry: None,
                retrying: false,
            })),
            source,
            model,
        }
    }

    pub fn phase(&self) -> Phase {
        self.state.lock().unwrap().phase.clone()
    }
    pub fn retry(&self) -> Option<Retry> {
        self.state.lock().unwrap().retry
    }
    pub fn set_retry(&self, retry: Option<Retry>) {
        let mut state = self.state.lock().unwrap();
        state.retry = retry;
        state.retrying = false;
    }
    pub(crate) fn start_retry(&self) {
        let mut state = self.state.lock().unwrap();
        state.retry = None;
        state.retrying = true;
    }
    pub(crate) fn retrying(&self) -> bool {
        self.state.lock().unwrap().retrying
    }
    pub fn from_kept(
        flow: FlowRef,
        source: SourceSelection,
        model: String,
        effort: Option<atman_runtime::provider::ReasoningSelection>,
    ) -> std::io::Result<Self> {
        if flow == source.flow || flow.session_id != source.flow.session_id {
            return Err(std::io::Error::other(
                "Question origin does not match retained Flow",
            ));
        }
        let mut question = Self::new(source, model);
        question.id = flow.instance_id.0.0;
        question.state.lock().unwrap().phase = Phase::Kept(flow);
        question.state.lock().unwrap().initial_effort = effort;
        Ok(question)
    }

    pub fn selected_model(&self) -> String {
        self.state.lock().unwrap().model.clone()
    }
    pub fn retention(&self) -> Option<atman_runtime::flow_store::retention::RetentionReceipt> {
        self.state.lock().unwrap().retention.clone()
    }
    pub fn set_retention(&self, receipt: atman_runtime::flow_store::retention::RetentionReceipt) {
        self.state.lock().unwrap().retention = Some(receipt);
    }
    pub fn retention_pending(&self) -> bool {
        self.state.lock().unwrap().retention_pending
    }
    pub fn set_retention_pending(&self, pending: bool) {
        self.state.lock().unwrap().retention_pending = pending;
    }
    pub fn set_model(&self, model: String) {
        self.state.lock().unwrap().model = model;
    }
    pub fn captured_source(&self) -> SourceSelection {
        self.state.lock().unwrap().captured.clone()
    }
    pub fn set_source_detail(&self, detail: String) {
        self.state.lock().unwrap().source_detail = detail;
    }
    pub(crate) fn source_detail(&self) -> String {
        self.state.lock().unwrap().source_detail.clone()
    }
    pub fn captured(&self, source: SourceSelection) -> bool {
        if !self.has_source(&source) {
            return false;
        }
        let mut state = self.state.lock().unwrap();
        state.captured = source;
        state.captured_at = Some(chrono::Utc::now());
        true
    }
    pub(crate) fn initialize(
        &self,
        effort: Option<atman_runtime::provider::ReasoningSelection>,
        excerpt: Option<String>,
        label: String,
    ) {
        let mut state = self.state.lock().unwrap();
        state.initial_effort = effort;
        state.excerpt = excerpt;
        state.source_label = label;
    }
    pub(crate) fn label(&self) -> String {
        self.state.lock().unwrap().source_label.clone()
    }
    pub(crate) fn capture_time(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .captured_at
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string()
            })
            .unwrap_or_default()
    }
    pub(crate) fn take_initial_input(
        &self,
    ) -> (
        Option<atman_runtime::provider::ReasoningSelection>,
        Option<String>,
    ) {
        let mut state = self.state.lock().unwrap();
        (state.initial_effort.take(), state.excerpt.take())
    }
    pub fn pending(&self) -> Option<Operation> {
        self.state
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .map(|(operation, _)| *operation)
    }
    pub fn begin(&self, operation: Operation, ticket: ControlTicket) -> bool {
        if ticket.binding.flow != self.address() {
            return false;
        }
        let mut state = self.state.lock().unwrap();
        if state.pending.is_some()
            || (operation == Operation::Keep && !matches!(state.phase, Phase::Transient(_)))
        {
            return false;
        }
        state.pending = Some((operation, ticket));
        true
    }
    pub fn finish(&self, operation: Operation, ticket: &ControlTicket) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.pending.as_ref() != Some(&(operation, ticket.clone())) {
            return false;
        }
        state.pending = None;
        true
    }

    pub fn address(&self) -> ViewAddress {
        match self.phase() {
            Phase::Draft => ViewAddress::QuestionDraft {
                session: self.source.flow.session_id.0,
                id: self.id,
            },
            Phase::Transient(flow) | Phase::Kept(flow) => flow.into(),
        }
    }

    /// Reuse follows the initial branch, not its changing commit cursor.
    pub fn has_source(&self, source: &SourceSelection) -> bool {
        self.source.flow == source.flow && self.source.branch == source.branch
    }

    pub fn begin_keep(&mut self, ticket: ControlTicket) -> bool {
        self.begin(Operation::Keep, ticket)
    }

    /// A failed request releases only its own attempt, so an older failure cannot cancel a retry.
    pub fn keep_failed(&mut self, ticket: &ControlTicket) -> bool {
        self.finish(Operation::Keep, ticket)
    }

    pub(crate) fn accepts(
        &self,
        kind: HandoffKind,
        request_id: Uuid,
        ticket: &ControlTicket,
        submission: Option<&ControlTicket>,
    ) -> bool {
        if ticket.request_id != request_id || ticket.binding.flow != self.address() {
            return false;
        }
        match kind {
            HandoffKind::Launch => {
                matches!(self.phase(), Phase::Draft) && submission == Some(ticket)
            }
            HandoffKind::Keep => {
                let state = self.state.lock().unwrap();
                matches!(state.phase, Phase::Transient(_))
                    && state.pending.as_ref() == Some(&(Operation::Keep, ticket.clone()))
            }
        }
    }

    pub(crate) fn accepts_destination(
        &self,
        kind: HandoffKind,
        ticket: &ControlTicket,
        destination: &ViewBinding,
    ) -> bool {
        destination.view_id == ticket.binding.view_id
            && ticket.binding.revision.checked_add(1) == Some(destination.revision)
            && destination.flow.session_id() == self.source.flow.session_id.0
            && destination.flow.instance().is_some()
            && destination.flow.instance() != Some(&self.source.flow)
            && match (kind, self.phase()) {
                (HandoffKind::Launch, Phase::Draft) => destination.flow != self.address(),
                (HandoffKind::Keep, Phase::Transient(flow)) => {
                    destination.flow.instance() == Some(&flow)
                }
                _ => false,
            }
    }

    pub(crate) fn installed(&mut self, kind: HandoffKind, flow: FlowRef) {
        let mut state = self.state.lock().unwrap();
        state.phase = match kind {
            HandoffKind::Launch => Phase::Transient(flow),
            HandoffKind::Keep => Phase::Kept(flow),
        };
        state.pending = None;
        state.retention_pending = false;
        state.retry = None;
        state.retrying = false;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_store::{FlowInstanceId, source_capture::ConversationBranchId},
    };

    pub(crate) fn question() -> QuickAsk {
        let flow = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: FlowInstanceId::now(),
        };
        let journal_id = Uuid::new_v4();
        QuickAsk::new(
            SourceSelection {
                branch: ConversationBranchId::initial(&flow, journal_id),
                flow,
                cursor: CommitReceipt {
                    journal_id,
                    sequence: 1,
                    digest: "source".into(),
                },
            },
            "vendor/model".into(),
        )
    }

    #[test]
    fn draft_identity_and_reuse_do_not_alias_the_source_or_its_cursor() {
        let question = question();
        assert_ne!(
            question.address(),
            ViewAddress::Instance(question.source.flow.clone())
        );
        assert!(question.address().instance().is_none());
        assert_eq!(
            question.address().session_id(),
            question.source.flow.session_id.0
        );
        let mut advanced = question.source.clone();
        advanced.cursor.sequence += 1;
        advanced.cursor.digest = "advanced".into();
        assert!(question.has_source(&advanced));
        advanced.branch = ConversationBranchId::initial(&advanced.flow, Uuid::new_v4());
        assert!(!question.has_source(&advanced));
    }

    #[test]
    fn question_clones_share_local_settings_and_publication_state() {
        let mut question = question();
        let clone = question.clone();
        clone.set_model("vendor/other".into());
        assert_eq!(question.selected_model(), "vendor/other");
        let target = FlowRef {
            session_id: question.source.flow.session_id,
            instance_id: FlowInstanceId::now(),
        };
        question.installed(HandoffKind::Launch, target.clone());
        assert_eq!(clone.phase(), Phase::Transient(target));
        clone.set_retention_pending(true);
        assert!(question.retention_pending());
        assert_eq!(question.source, clone.source);
        let restored = QuickAsk::from_kept(
            FlowRef {
                session_id: question.source.flow.session_id,
                instance_id: FlowInstanceId::now(),
            },
            question.source.clone(),
            "vendor/kept".into(),
            None,
        )
        .unwrap();
        assert!(matches!(restored.phase(), Phase::Kept(_)));
        assert!(
            restored.capture_time().is_empty(),
            "restore cannot fabricate capture time"
        );
    }

    #[test]
    fn handoff_requires_exact_attempt_and_next_binding() {
        let mut question = question();
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::new_v4(),
                revision: 2,
                flow: question.address(),
            },
            request_id: Uuid::new_v4(),
        };
        assert!(question.accepts(
            HandoffKind::Launch,
            ticket.request_id,
            &ticket,
            Some(&ticket)
        ));
        assert!(!question.accepts(HandoffKind::Launch, Uuid::new_v4(), &ticket, Some(&ticket)));
        assert!(!question.accepts(HandoffKind::Launch, ticket.request_id, &ticket, None));
        let destination = ViewBinding {
            view_id: ticket.binding.view_id,
            revision: 3,
            flow: FlowRef {
                session_id: question.source.flow.session_id,
                instance_id: FlowInstanceId::now(),
            }
            .into(),
        };
        assert!(question.accepts_destination(HandoffKind::Launch, &ticket, &destination));
        assert!(!question.accepts_destination(
            HandoffKind::Launch,
            &ticket,
            &ViewBinding {
                revision: 2,
                ..destination.clone()
            }
        ));
        assert!(!question.accepts_destination(
            HandoffKind::Launch,
            &ticket,
            &ViewBinding {
                view_id: Uuid::new_v4(),
                ..destination.clone()
            }
        ));
        assert!(!question.accepts_destination(
            HandoffKind::Launch,
            &ticket,
            &ViewBinding {
                flow: question.source.flow.clone().into(),
                ..destination.clone()
            }
        ));
        question.installed(
            HandoffKind::Launch,
            destination.flow.instance().unwrap().clone(),
        );
        let keep = ControlTicket {
            binding: destination,
            request_id: Uuid::new_v4(),
        };
        let kept = ViewBinding {
            revision: keep.binding.revision + 1,
            ..keep.binding.clone()
        };
        assert!(question.accepts_destination(HandoffKind::Keep, &keep, &kept));
        assert!(!question.accepts_destination(HandoffKind::Launch, &keep, &kept));
        assert!(
            !question.accepts_destination(
                HandoffKind::Keep,
                &keep,
                &ViewBinding {
                    flow: FlowRef {
                        session_id: question.source.flow.session_id,
                        instance_id: FlowInstanceId::now()
                    }
                    .into(),
                    ..kept
                }
            )
        );
        assert!(!question.accepts(HandoffKind::Keep, keep.request_id, &keep, None));
        assert!(question.begin_keep(keep.clone()));
        assert!(question.accepts(HandoffKind::Keep, keep.request_id, &keep, None));
        assert!(!question.keep_failed(&ControlTicket {
            request_id: Uuid::new_v4(),
            ..keep.clone()
        }));
        assert!(question.accepts(HandoffKind::Keep, keep.request_id, &keep, None));
        assert!(question.keep_failed(&keep));
        assert!(!question.accepts(HandoffKind::Keep, keep.request_id, &keep, None));
    }
}
