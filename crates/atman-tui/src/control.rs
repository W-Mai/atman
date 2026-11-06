//! View-bound host commands. A queued operation retains its original destination and reply channel.

use atman_runtime::event_log::legacy::LegacyFlowKey;
use atman_runtime::flow_store::FlowRef;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{TuiCommand, TuiControl};

mod work;
pub(crate) use work::{Completion, ControlWork};

fn durable(command: &TuiControl) -> bool {
    match command {
        TuiControl::SearchHistory { .. }
        | TuiControl::ListSessionChoices
        | TuiControl::OpenLegacySession(_)
        | TuiControl::OpenSession(_)
        | TuiControl::BrowseFlows(_)
        | TuiControl::FocusFlow { .. }
        | TuiControl::SwitchSession { .. }
        | TuiControl::OpenAliasManager { .. }
        | TuiControl::TestProviderConnection(_)
        | TuiControl::TestModel(_)
        | TuiControl::TermResize { .. }
        | TuiControl::McpTest { .. }
        | TuiControl::McpListResources { .. }
        | TuiControl::McpListPrompts { .. }
        | TuiControl::ListKnowledge { .. }
        | TuiControl::ReloadRules { .. }
        | TuiControl::GetConfessionHistory { .. }
        | TuiControl::SuggestOrganization { .. }
        | TuiControl::CancelOrganization => false,
        TuiControl::Projects(request) => request.mutation.is_some(),
        TuiControl::Submit(submission) => {
            submission.control_command().map(str::trim) != Some(":suggest")
        }
        _ => true,
    }
}

/// A view location, independent of the flow's execution or default designation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum FlowPlacement {
    #[default]
    Main,
    Floating,
}

/// A display destination. Original history addresses carry no execution authority or persistent instance identity.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ViewAddress {
    Instance(FlowRef),
    Legacy { session: Uuid, owner: LegacyFlowKey },
}

impl From<FlowRef> for ViewAddress {
    fn from(flow: FlowRef) -> Self {
        Self::Instance(flow)
    }
}

impl ViewAddress {
    pub fn session_target(&self) -> crate::project_hub::SessionTarget {
        match self {
            Self::Instance(flow) => {
                crate::project_hub::SessionTarget::Persistent(flow.session_id.0)
            }
            Self::Legacy { session, .. } => {
                crate::project_hub::SessionTarget::Legacy(session.to_string())
            }
        }
    }
    pub fn directory(&self) -> crate::flow_hub::HubRequest {
        match self {
            Self::Instance(flow) => crate::flow_hub::HubRequest::SessionOverview {
                session: flow.session_id.0,
                after: None,
            },
            Self::Legacy { session, .. } => crate::flow_hub::HubRequest::LegacyFlows {
                session: *session,
                after: None,
            },
        }
    }
    pub fn session_id(&self) -> Uuid {
        match self {
            Self::Instance(flow) => flow.session_id.0,
            Self::Legacy { session, .. } => *session,
        }
    }
    pub fn instance(&self) -> Option<&FlowRef> {
        match self {
            Self::Instance(flow) => Some(flow),
            Self::Legacy { .. } => None,
        }
    }
    pub fn label(&self) -> String {
        match self {
            Self::Instance(flow) => format!("Flow {}", flow.instance_id),
            Self::Legacy { owner, .. } => match owner {
                LegacyFlowKey::Default => "Default flow history".into(),
                LegacyFlowKey::Agent { owner } => format!("Agent history · {owner}"),
                LegacyFlowKey::SpawnedRun { run_id } => format!("Flow history · {run_id}"),
                LegacyFlowKey::Unassigned => "Unassigned history".into(),
            },
        }
    }
}

/// A client view's selection, independent of the instance's execution revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewBinding {
    pub view_id: Uuid,
    pub revision: u64,
    pub flow: ViewAddress,
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
    work: ControlWork,
    completion: std::sync::Arc<std::sync::OnceLock<Option<std::sync::Arc<Completion>>>>,
}

impl ControlReplies {
    /// Retains this request while it publishes a mutation. Read-only preparation does not delay exit; publication cannot start after workspace shutdown admission closes. Clones share a single completion guard.
    pub fn begin_publication(&self) -> Option<Self> {
        self.completion
            .get_or_init(|| self.work.begin_publication())
            .as_ref()?;
        Some(self.clone())
    }

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
        work: ControlWork,
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
                work: ControlWork::default(),
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

    pub(crate) fn control_work(&self) -> Option<ControlWork> {
        match &self.0 {
            Transport::Flow { work, .. } => Some(work.clone()),
            Transport::Session(_) => None,
        }
    }

    pub(crate) fn bind_work(&mut self, shared: ControlWork) {
        if let Transport::Flow { work, .. } = &mut self.0 {
            *work = shared;
        }
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
                work,
            } => {
                let ticket = ControlTicket {
                    binding: binding.clone(),
                    request_id: Uuid::now_v7(),
                };
                let destination = ControlReplies {
                    ticket: ticket.clone(),
                    tx: replies.clone(),
                    work: work.clone(),
                    completion: std::sync::Arc::new(if durable(&command) {
                        std::sync::OnceLock::from(Some(work.begin()))
                    } else {
                        std::sync::OnceLock::new()
                    }),
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

    #[tokio::test]
    async fn conditional_publication_shares_one_guard_and_cannot_start_after_shutdown() {
        let (host, mut incoming) = mpsc::unbounded_channel();
        let (sender, _observation) = ControlSender::for_flow(
            ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: ViewAddress::Legacy {
                    session: Uuid::now_v7(),
                    owner: LegacyFlowKey::Default,
                },
            },
            host,
        );
        let work = sender.control_work().unwrap();
        let suggest = |text: &str| {
            TuiControl::Submit(crate::TuiSubmission {
                text: text.into(),
                images: vec![],
                reasoning: None,
                presentation: None,
            })
        };
        sender.send(suggest(":suggest")).unwrap();
        sender.send(suggest(" :suggest  \n")).unwrap();
        let waiting = incoming.recv().await.unwrap();
        let never_started = incoming.recv().await.unwrap();
        assert!(
            !work.is_waiting(),
            "model and confirmation preparation must not hold exit"
        );
        let publication = waiting.replies.begin_publication().unwrap();
        let shared = waiting.replies.begin_publication().unwrap();
        assert!(std::sync::Arc::ptr_eq(
            publication.completion.get().unwrap().as_ref().unwrap(),
            shared.completion.get().unwrap().as_ref().unwrap(),
        ));
        work.close_publications();
        assert!(never_started.replies.begin_publication().is_none());
        drop(waiting);
        drop(shared);
        assert!(work.is_waiting());
        drop(publication);
        work.settled().await;
        assert!(!work.is_waiting());
        assert!(never_started.replies.begin_publication().is_none());
    }

    #[tokio::test]
    async fn durable_replies_retain_one_request_until_the_actual_worker_finishes() {
        let (host, mut incoming) = mpsc::unbounded_channel();
        let binding = ViewBinding {
            view_id: Uuid::now_v7(),
            revision: 1,
            flow: ViewAddress::Legacy {
                session: Uuid::now_v7(),
                owner: LegacyFlowKey::Default,
            },
        };
        let (sender, observation) = ControlSender::for_flow(binding, host);
        let work = sender.control_work().unwrap();
        sender
            .send(TuiControl::MutateSettings(
                crate::settings::SettingsMutationRequest {
                    id: Uuid::now_v7(),
                    action: crate::settings::SettingsMutation::MathRendering { enabled: false },
                },
            ))
            .unwrap();
        let request = incoming.recv().await.unwrap();
        let completion = request.replies.clone();
        drop(request);
        drop(observation);
        assert!(work.is_waiting());
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let job = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _completion = completion;
                started.send(()).unwrap();
                let _ = blocked.recv();
            })
            .await
            .unwrap();
        });
        entered.await.unwrap();
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        assert!(
            work.is_waiting(),
            "dropping a poll cannot finish actual file publication"
        );
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), work.settled())
            .await
            .unwrap();
        sender.send(TuiControl::ListSessionChoices).unwrap();
        let read = incoming.recv().await.unwrap();
        assert!(!work.is_waiting(), "read navigation does not delay exit");
        drop(read);
        drop(incoming);
        assert!(sender.send(TuiControl::CancelFlow).is_err());
        assert!(!work.is_waiting(), "failed admission releases its lease");
    }

    #[test]
    fn queued_commands_and_replies_retain_their_view_binding() {
        let (host, mut incoming) = mpsc::unbounded_channel();
        let a = ViewBinding {
            view_id: Uuid::new_v4(),
            revision: 1,
            flow: FlowRef {
                session_id: Uuid::new_v4().into(),
                instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
            }
            .into(),
        };
        let b = ViewBinding {
            revision: 2,
            flow: FlowRef {
                instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
                ..a.flow.instance().unwrap().clone()
            }
            .into(),
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
