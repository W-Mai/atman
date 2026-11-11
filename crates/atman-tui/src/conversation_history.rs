//! View-bound conversation history intents and owner receipts.

use atman_runtime::flow_store::{
    branches::{HistoryBranchPage, HistoryHead},
    history_operation::{
        HistoryOperationRecord, HistoryOperationRequest, HistoryOperationReview,
        HistoryOperationTarget,
    },
};
use uuid::Uuid;
mod ui;
pub(crate) use ui::{Intent, State, capture_turn, update};

pub struct PreparedRecovery {
    pub operation: Uuid,
    pub head: HistoryHead,
    pub input: Uuid,
    pub(crate) recoverable_inputs: Vec<Uuid>,
    pub(crate) editor: crate::input::InputEditor,
    pub(crate) quote: Option<std::sync::Arc<str>>,
}

impl std::fmt::Debug for PreparedRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRecovery")
            .field("operation", &self.operation)
            .field("head", &self.head)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

impl PreparedRecovery {
    /// Prepare potentially large authored text away from the UI event and render paths.
    pub fn prepare(
        recovered: atman_runtime::flow_store::history_operation::RecoveredHistoryInput,
    ) -> Self {
        let mut editor = crate::input::InputEditor::default();
        let content = recovered.content;
        let quote = if let Some(presentation) = content.presentation {
            editor.ingest_paste(&presentation.prompt);
            presentation.quote.map(|quote| quote.text.into())
        } else {
            editor.ingest_paste(&content.text);
            None
        };
        for image in content.images {
            editor.attach_image(image);
        }
        Self {
            operation: recovered.operation,
            head: recovered.head,
            input: recovered.input,
            recoverable_inputs: recovered.recoverable_inputs,
            editor,
            quote,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Command {
    Branches {
        expected: HistoryHead,
        offset: u64,
    },
    Review {
        id: Uuid,
        expected: HistoryHead,
        target: HistoryOperationTarget,
    },
    Confirm(Box<HistoryOperationRequest>),
    Status(Uuid),
    Pending,
    Recover {
        operation: Uuid,
        input: Uuid,
    },
    Abort(Uuid),
}

impl Command {
    pub(crate) fn durable(&self) -> bool {
        matches!(self, Self::Confirm(_) | Self::Abort(_))
    }
}

#[derive(Debug)]
pub enum Response {
    Branches(Box<HistoryBranchPage>),
    Review(Box<HistoryOperationReview>),
    Operation(Option<Box<HistoryOperationRecord>>),
    Recovered(Box<PreparedRecovery>),
}

/// The same ticket receives progress and the final outcome; hidden views retain their owner.
#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) ticket: Option<crate::control::ControlTicket>,
    pub(crate) response: Option<Result<Response, String>>,
    pub(crate) running: bool,
}

impl Pending {
    pub(crate) fn receive(
        &mut self,
        ticket: Option<&crate::control::ControlTicket>,
        result: Result<Response, String>,
        finished: bool,
    ) {
        if self.ticket.is_some() && self.ticket.as_ref() == ticket {
            crate::render_work::discard(self.response.replace(result));
            self.running = !finished;
        } else {
            crate::render_work::discard(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ControlSender, ViewBinding};
    use atman_runtime::flow_store::{FlowInstanceId, FlowRef};

    #[tokio::test]
    async fn history_receipts_keep_the_original_view_and_observed_head() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let (tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let (sender, mut replies) = ControlSender::for_flow(
            ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            tx,
        );
        let head = HistoryHead::initial(&flow, Uuid::now_v7());
        sender.observe_history(&flow, Some(&head));
        let ticket = sender
            .send_tracked(crate::TuiControl::ConversationHistory(Command::Branches {
                expected: head.clone(),
                offset: 0,
            }))
            .unwrap()
            .unwrap();
        let request = requests.recv().await.unwrap();
        assert_eq!(request.ticket.history_head, Some(head));
        let mut pending = Pending {
            ticket: Some(ticket.clone()),
            running: true,
            ..Default::default()
        };
        sender.observe_history(&flow, None);
        request
            .replies
            .send(crate::TuiCommand::ConversationHistory {
                result: Ok(Response::Operation(None)),
                finished: false,
            })
            .unwrap();
        let reply = replies.recv().await.unwrap();
        let crate::TuiCommand::ConversationHistory { result, finished } = reply.command else {
            panic!("history reply")
        };
        pending.receive(Some(&reply.ticket), result, finished);
        assert!(pending.running);
        let foreign = crate::control::ControlTicket {
            request_id: Uuid::now_v7(),
            ..ticket.clone()
        };
        pending.receive(Some(&foreign), Err("another request".into()), true);
        assert!(pending.running);
        assert!(pending.response.as_ref().unwrap().is_ok());
        pending.receive(Some(&ticket), Ok(Response::Operation(None)), true);
        assert!(!pending.running);
    }
}
