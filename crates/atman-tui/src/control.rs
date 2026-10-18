//! View-bound host commands. A queued operation retains its original destination and reply channel.

use atman_runtime::flow_store::FlowRef;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{TuiCommand, TuiControl};

/// A view location, independent of the flow's execution or default designation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum FlowPlacement {
    #[default]
    Main,
    Floating,
}

/// A client view's selection, independent of the instance's execution revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewBinding {
    pub view_id: Uuid,
    pub revision: u64,
    pub flow: FlowRef,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlTicket {
    pub binding: ViewBinding,
    pub request_id: Uuid,
}

pub struct ControlReply {
    pub ticket: ControlTicket,
    pub command: TuiCommand,
}

/// The host authenticates and authorizes the command before dispatch. The binding is a destination, not an authority grant.
pub struct BoundControl {
    pub ticket: ControlTicket,
    pub command: TuiControl,
    pub replies: ControlReplies,
}

/// An immutable reply address captured with the original request.
#[derive(Clone)]
pub struct ControlReplies {
    ticket: ControlTicket,
    tx: mpsc::UnboundedSender<ControlReply>,
}

impl ControlReplies {
    pub fn send(
        &self,
        command: TuiCommand,
    ) -> Result<(), mpsc::error::SendError<Box<ControlReply>>> {
        self.tx
            .send(ControlReply {
                ticket: self.ticket.clone(),
                command,
            })
            .map_err(|error| mpsc::error::SendError(Box::new(error.0)))
    }
}

#[derive(Clone)]
pub struct ControlSender(Transport);

#[derive(Clone)]
enum Transport {
    Session(mpsc::UnboundedSender<TuiControl>),
    Flow {
        binding: ViewBinding,
        tx: mpsc::UnboundedSender<BoundControl>,
        replies: mpsc::UnboundedSender<ControlReply>,
    },
}

impl From<mpsc::UnboundedSender<TuiControl>> for ControlSender {
    fn from(tx: mpsc::UnboundedSender<TuiControl>) -> Self {
        Self(Transport::Session(tx))
    }
}

impl ControlSender {
    /// Existing session hosts retain their direct command receiver.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<TuiControl>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx.into(), rx)
    }

    /// Each binding owns its reply mailbox. Retain it with that view when focus changes.
    pub fn for_flow(
        binding: ViewBinding,
        tx: mpsc::UnboundedSender<BoundControl>,
    ) -> (Self, mpsc::UnboundedReceiver<ControlReply>) {
        let (replies, rx) = mpsc::unbounded_channel();
        (
            Self(Transport::Flow {
                binding,
                tx,
                replies,
            }),
            rx,
        )
    }

    pub fn binding(&self) -> Option<&ViewBinding> {
        match &self.0 {
            Transport::Session(_) => None,
            Transport::Flow { binding, .. } => Some(binding),
        }
    }

    pub fn send(&self, command: TuiControl) -> Result<(), mpsc::error::SendError<Box<TuiControl>>> {
        self.send_tracked(command).map(|_| ())
    }

    /// Returns the request identity for a flow command. Failure returns the original payload so the view can retain its draft and attachments.
    pub fn send_tracked(
        &self,
        command: TuiControl,
    ) -> Result<Option<ControlTicket>, mpsc::error::SendError<Box<TuiControl>>> {
        match &self.0 {
            Transport::Session(tx) => tx
                .send(command)
                .map(|_| None)
                .map_err(|error| mpsc::error::SendError(Box::new(error.0))),
            Transport::Flow {
                binding,
                tx,
                replies,
            } => {
                let ticket = ControlTicket {
                    binding: binding.clone(),
                    request_id: Uuid::now_v7(),
                };
                let destination = ControlReplies {
                    ticket: ticket.clone(),
                    tx: replies.clone(),
                };
                tx.send(BoundControl {
                    ticket: ticket.clone(),
                    command,
                    replies: destination,
                })
                .map_err(|error| mpsc::error::SendError(Box::new(error.0.command)))?;
                Ok(Some(ticket))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_commands_and_replies_retain_their_view_binding() {
        let (host, mut incoming) = mpsc::unbounded_channel();
        let a = ViewBinding {
            view_id: Uuid::new_v4(),
            revision: 1,
            flow: FlowRef {
                session_id: Uuid::new_v4().into(),
                instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
            },
        };
        let b = ViewBinding {
            revision: 2,
            flow: FlowRef {
                instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
                ..a.flow.clone()
            },
            ..a.clone()
        };
        let (first, mut first_replies) = ControlSender::for_flow(a.clone(), host.clone());
        let retained = first.clone();
        let first_ticket = first.send_tracked(TuiControl::CancelFlow).unwrap().unwrap();
        let (selected, mut selected_replies) = ControlSender::for_flow(b.clone(), host);
        selected.send(TuiControl::HardStop).unwrap();
        retained.send(TuiControl::CompactNow).unwrap();
        let old = incoming.try_recv().unwrap();
        assert_eq!(old.ticket, first_ticket);
        assert_eq!(old.ticket.binding, a);
        old.replies
            .send(TuiCommand::GoalSaved {
                request_id: 1,
                result: Ok(()),
            })
            .unwrap();
        assert!(selected_replies.try_recv().is_err());
        assert_eq!(first_replies.try_recv().unwrap().ticket, first_ticket);
        let current = incoming.try_recv().unwrap();
        assert_eq!(current.ticket.binding, b);
        let retained = incoming.try_recv().unwrap();
        assert_eq!(retained.ticket.binding, a);
        assert_ne!(retained.ticket.request_id, first_ticket.request_id);
        drop(selected_replies);
        assert!(
            current
                .replies
                .send(TuiCommand::GoalSaved {
                    request_id: 1,
                    result: Ok(())
                })
                .is_err()
        );
        assert!(first_replies.try_recv().is_err());
        drop(incoming);
        let error = selected.send(TuiControl::CancelFlow).unwrap_err();
        assert!(matches!(*error.0, TuiControl::CancelFlow));
    }
}
