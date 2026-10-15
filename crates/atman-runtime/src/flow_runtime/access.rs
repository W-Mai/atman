//! Invocation-bound peer operations with live capability checks.

use std::{
    io,
    sync::{Arc, RwLock},
};

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease, TurnPhase};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use uuid::Uuid;

use super::{host::RuntimeHost, send::SendAttempt};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::access::{FlowAction, SessionAccess};
use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity};
use crate::flow_store::publication::PublishedMessage;
use crate::flow_store::{FlowRef, FlowTransition, invalid};
use crate::message::{Message, MessageOrigin};

/// Peer controls cannot remove another controller's pause key or forge lifecycle completion.
#[derive(Clone, Copy, Debug)]
pub enum PeerControl {
    Pause,
    Resume,
    CancelTurn,
}

/// An opaque host-issued identity for one admitted invocation, not a serializable model argument.
#[derive(Clone)]
pub struct FlowClient {
    host: RuntimeHost,
    lease: TurnLease<AtmanUuid>,
    state: watch::Receiver<FlowInstance<AtmanUuid>>,
    access: Arc<RwLock<SessionAccess>>,
}

impl RuntimeHost {
    /// Bind only after authenticating the invocation in the trusted execution adapter.
    pub async fn bind_invocation(&self, lease: TurnLease<AtmanUuid>) -> io::Result<FlowClient> {
        let source = self.flow(&lease.flow).await?;
        let client = FlowClient {
            access: self.session(lease.flow.session_id.0)?.access,
            host: self.clone(),
            state: source.subscribe_state(),
            lease,
        };
        client.authenticate()?;
        Ok(client)
    }
}

impl FlowClient {
    pub fn source(&self) -> &FlowRef {
        &self.lease.flow
    }

    fn authenticate(&self) -> io::Result<()> {
        if self.state.has_changed().is_err()
            || !self
                .state
                .borrow()
                .can_receive_at_boundary(&self.lease)
                .unwrap_or(false)
        {
            return Err(denied(
                "invocation is no longer admitted for peer operations",
            ));
        }
        Ok(())
    }

    fn authorize(&self, target: &FlowRef, action: FlowAction) -> io::Result<()> {
        self.authenticate()?;
        self.authorize_delivery(target, action)
    }

    fn authorize_delivery(&self, target: &FlowRef, action: FlowAction) -> io::Result<()> {
        let session = self.host.session(self.lease.flow.session_id.0)?;
        if session.is_stopped() || !Arc::ptr_eq(&session.access, &self.access) {
            return Err(denied("source access owner is unavailable or replaced"));
        }
        if self
            .access
            .read()
            .unwrap()
            .allows(&self.lease.flow, target, action)
        {
            Ok(())
        } else {
            Err(denied("flow action is outside the current access grants"))
        }
    }

    pub async fn inspect(&self, target: FlowRef) -> io::Result<FlowInstance<AtmanUuid>> {
        self.authorize(&target, FlowAction::Discover)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::Discover)?;
                Ok(store.state().clone())
            })
            .await
    }

    pub async fn read_output(&self, target: FlowRef) -> io::Result<Vec<PublishedMessage>> {
        self.authorize(&target, FlowAction::ReadOutput)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::ReadOutput)?;
                Ok(store
                    .published_messages()
                    .iter()
                    .filter(|record| {
                        record.publication.context_run.is_none()
                            && record.publication.message.origin == MessageOrigin::FinalAnswer
                    })
                    .cloned()
                    .collect())
            })
            .await
    }

    pub async fn read_context(&self, target: FlowRef) -> io::Result<Vec<Message>> {
        self.authorize(&target, FlowAction::ReadContext)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::ReadContext)?;
                Ok(store.context_messages())
            })
            .await
    }

    pub async fn send(
        &self,
        target: FlowRef,
        request_id: Uuid,
        text: String,
        delivery: DeliveryMode,
    ) -> io::Result<SendAttempt> {
        self.authorize(&target, FlowAction::Send)?;
        let source = self.clone();
        let receiver = self.clone();
        self.host
            .send(
                self.lease.clone(),
                DeliveryRequest {
                    request_id,
                    sender: SenderIdentity::Flow(self.lease.flow.clone()),
                    recipient: target,
                    delivery,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Interjection { text },
                },
                None,
                move |request| source.authorize(&request.recipient, FlowAction::Send),
                // A committed send can outlive its originating turn, but new receiver admission still checks current grants.
                move |request| receiver.authorize_delivery(&request.recipient, FlowAction::Send),
            )
            .await
    }

    pub async fn control(
        &self,
        target: FlowRef,
        expected_revision: u64,
        control: PeerControl,
    ) -> io::Result<CommitReceipt> {
        self.authorize(&target, FlowAction::Control)?;
        let client = self.clone();
        let key = pause_key(&self.lease.flow, &target);
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::Control)?;
                if store.state().revision() != expected_revision {
                    return Err(invalid("target execution revision changed"));
                }
                let transition = match control {
                    PeerControl::Pause => FlowTransition::ExecutionPaused { key },
                    PeerControl::Resume => FlowTransition::ExecutionResumed { key },
                    PeerControl::CancelTurn => match store.state().state() {
                        InstanceState::Open(InstanceActivity::Active(active)) => {
                            FlowTransition::PhaseChanged {
                                lease: active.lease.clone(),
                                phase: TurnPhase::Cancelling,
                            }
                        }
                        _ => return Ok(store.cursor().clone()),
                    },
                };
                store.transition(expected_revision, transition)
            })
            .await
    }
}

fn pause_key(source: &FlowRef, target: &FlowRef) -> AtmanUuid {
    let mut hash = Sha256::new();
    hash.update(b"atman.peer.execution-pause\0");
    for id in [
        source.session_id.0,
        source.instance_id.0.0,
        target.session_id.0,
        target.instance_id.0.0,
    ] {
        hash.update(id.as_bytes());
    }
    let digest = hash.finalize();
    let mut key = [0; 16];
    key.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(key).into()
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
