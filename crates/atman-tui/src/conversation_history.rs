//! View-bound conversation history intents and owner receipts.

use atman_runtime::flow_store::{
    branches::{HistoryBranchPage, HistoryHead},
    history_operation::{
        HistoryOperationRecord, HistoryOperationRequest, HistoryOperationReview,
        HistoryOperationTarget,
    },
};
use uuid::Uuid;

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
    Recovered(Box<atman_runtime::flow_store::history_operation::RecoveredHistoryInput>),
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
            self.response = Some(result);
            self.running = !finished;
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
