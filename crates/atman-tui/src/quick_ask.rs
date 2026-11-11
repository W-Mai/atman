//! Question identity and operation ownership, independent of a displayed source document.

use atman_runtime::flow_store::{FlowRef, source_capture::SourceSelection};
use uuid::Uuid;

use crate::control::{ControlTicket, ViewAddress, ViewBinding};

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
    phase: Phase,
    pending_keep: Option<ControlTicket>,
}

impl QuickAsk {
    pub fn new(source: SourceSelection, model: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            source,
            model,
            phase: Phase::Draft,
            pending_keep: None,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn address(&self) -> ViewAddress {
        match &self.phase {
            Phase::Draft => ViewAddress::QuestionDraft {
                session: self.source.flow.session_id.0,
                id: self.id,
            },
            Phase::Transient(flow) | Phase::Kept(flow) => flow.clone().into(),
        }
    }

    /// Reuse follows the initial branch, not its changing commit cursor.
    pub fn has_source(&self, source: &SourceSelection) -> bool {
        self.source.flow == source.flow && self.source.branch == source.branch
    }

    pub fn begin_keep(&mut self, ticket: ControlTicket) -> bool {
        if !matches!(self.phase, Phase::Transient(_))
            || self.pending_keep.is_some()
            || ticket.binding.flow != self.address()
        {
            return false;
        }
        self.pending_keep = Some(ticket);
        true
    }

    /// A failed request releases only its own attempt, so an older failure cannot cancel a retry.
    pub fn keep_failed(&mut self, ticket: &ControlTicket) -> bool {
        if self.pending_keep.as_ref() != Some(ticket) {
            return false;
        }
        self.pending_keep = None;
        true
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
            HandoffKind::Launch => matches!(self.phase, Phase::Draft) && submission == Some(ticket),
            HandoffKind::Keep => {
                matches!(self.phase, Phase::Transient(_))
                    && self.pending_keep.as_ref() == Some(ticket)
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
            && match (kind, &self.phase) {
                (HandoffKind::Launch, Phase::Draft) => destination.flow != self.address(),
                (HandoffKind::Keep, Phase::Transient(flow)) => {
                    destination.flow.instance() == Some(flow)
                }
                _ => false,
            }
    }

    pub(crate) fn installed(&mut self, kind: HandoffKind, flow: FlowRef) {
        self.phase = match kind {
            HandoffKind::Launch => Phase::Transient(flow),
            HandoffKind::Keep => Phase::Kept(flow),
        };
        self.pending_keep = None;
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
