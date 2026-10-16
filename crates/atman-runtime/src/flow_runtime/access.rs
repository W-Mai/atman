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
use crate::flow_store::subscription::{AnswerSubscription, SubscriptionRecord};
use crate::flow_store::{FlowInstanceId, FlowRef, FlowTransition, invalid};
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

/// A bounded catalog page. Execution states are sampled independently of catalog membership.
pub struct InstancePage {
    pub instances: Vec<InstanceSummary>,
    pub next: Option<FlowInstanceId>,
    pub total: usize,
    pub catalog: CommitReceipt,
}

pub struct InstanceSummary {
    pub address: FlowRef,
    pub entry: Option<String>,
    pub state: Option<FlowInstance<AtmanUuid>>,
    /// A registered instance remains discoverable when its journal cannot be opened.
    pub unavailable: Option<String>,
}

impl RuntimeHost {
    /// Bind only after authenticating the invocation in the trusted execution adapter.
    pub async fn bind_invocation(&self, lease: TurnLease<AtmanUuid>) -> io::Result<FlowClient> {
        let source = self.flow(&lease.flow).await?;
        FlowClient::bind(self.clone(), &source, lease)
    }
}

impl FlowClient {
    /// Product spawn entry: creation stays in the authenticated caller's session. Tool admission supplies the execution ceiling; provenance never grants control over the new instance.
    pub(crate) async fn spawn(
        &self,
        descriptor: crate::flow_store::FlowDescriptor,
        input: crate::flow_store::launch::LaunchInput,
        execution: super::launch::LaunchExecution,
    ) -> io::Result<super::launch::LaunchedFlow> {
        self.authorize(self.source(), FlowAction::Send)?;
        if descriptor.address.session_id != self.source().session_id
            || descriptor.created_by.as_ref() != Some(self.source())
            || input.request.sender != SenderIdentity::Flow(self.source().clone())
            || input
                .result_route
                .as_ref()
                .is_some_and(|route| &route.recipient != self.source())
        {
            return Err(denied("spawn identity does not match its admitted caller"));
        }
        let client = self.clone();
        self.host
            .launch_owned(
                descriptor,
                input,
                move |_, _| client.authorize(client.source(), FlowAction::Send),
                Some(execution),
            )
            .await
    }

    pub(super) fn bind(
        host: RuntimeHost,
        source: &super::FlowWorker,
        lease: TurnLease<AtmanUuid>,
    ) -> io::Result<Self> {
        let state = source.subscribe_state();
        if source.address() != &lease.flow
            || state.has_changed().is_err()
            || !matches!(state.borrow().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == lease)
        {
            return Err(denied("peer client requires an admitted instance turn"));
        }
        // Binding is identity, not operation admission. Pauses are checked when using the client.
        Ok(Self {
            access: host.session(lease.flow.session_id.0)?.access,
            host,
            state,
            lease,
        })
    }

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

    /// Discover the current session, newest registration first. A stable instance cursor avoids repeating earlier entries when another flow is registered between pages. This does not expose private message history, start execution, or grant authority to control the listed instances.
    pub async fn instances(
        &self,
        before: Option<FlowInstanceId>,
        limit: usize,
    ) -> io::Result<InstancePage> {
        self.authorize(self.source(), FlowAction::Discover)?;
        if !(1..=50).contains(&limit) {
            return Err(invalid("instance page limit must be between 1 and 50"));
        }
        let session = self.host.session(self.source().session_id.0)?;
        let snapshot = session.snapshot().await?;
        let registrations = &snapshot.catalog.flows;
        let end = match before {
            Some(id) => registrations
                .iter()
                .position(|entry| entry.id == id)
                .ok_or_else(|| invalid("instance page cursor is not in this session"))?,
            None => registrations.len(),
        };
        let start = end.saturating_sub(limit);
        let mut instances = Vec::with_capacity(end - start);
        for registration in registrations[start..end].iter().rev() {
            let address = FlowRef {
                session_id: self.source().session_id,
                instance_id: registration.id.clone(),
            };
            self.authorize(&address, FlowAction::Discover)?;
            let summary = session.describe_flow(registration.id.clone()).await;
            // An expired caller must not turn a failed authorization into an unavailable row.
            self.authorize(&address, FlowAction::Discover)?;
            let (entry, state, unavailable) = match summary {
                Ok((entry, state)) => (Some(entry), Some(state), None),
                Err(error) => (None, None, Some(error.to_string())),
            };
            instances.push(InstanceSummary {
                address,
                entry,
                state,
                unavailable,
            });
        }
        self.authorize(self.source(), FlowAction::Discover)?;
        Ok(InstancePage {
            next: (start > 0).then(|| registrations[start].id.clone()),
            instances,
            total: registrations.len(),
            catalog: snapshot.cursor,
        })
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

    /// Read one committed entry result under the same authority as public output.
    /// An absent record is not evidence that an invocation succeeded or failed.
    pub async fn read_result(
        &self,
        target: FlowRef,
        turn: Option<crate::event::TurnId>,
    ) -> io::Result<Option<crate::flow_store::completion::CommittedCompletion>> {
        self.authorize(&target, FlowAction::ReadOutput)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::ReadOutput)?;
                Ok(store.completion(turn.as_ref()).cloned())
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

    /// Register before returning. The subscription outlives this invocation and keeps its original
    /// source position on retry; current watch grants are rechecked for every subsequent delivery.
    pub async fn watch_answers(
        &self,
        subscription: AnswerSubscription,
    ) -> io::Result<SubscriptionRecord> {
        self.authenticate()?;
        let recipient = self.host.flow(self.source()).await?;
        let client = self.clone();
        let requested = subscription.clone();
        if let Some(record) = recipient
            .request(move |store| {
                client.authenticate()?;
                existing_subscription(store, &requested)
            })
            .await?
        {
            return Ok(record);
        }
        self.authorize(&subscription.source, FlowAction::Watch)?;
        let client = self.clone();
        let anchor = self
            .host
            .flow(&subscription.source)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::Watch)?;
                Ok(store.publication_anchor())
            })
            .await?;
        let client = self.clone();
        recipient
            .request(move |store| {
                client.authenticate()?;
                // Concurrent identical registrations can acquire different anchors. Preserve the first.
                if let Some(record) = existing_subscription(store, &subscription)? {
                    return Ok(record);
                }
                client.authorize(&subscription.source, FlowAction::Watch)?;
                store.subscribe_answers(subscription.clone(), &anchor)?;
                existing_subscription(store, &subscription)?
                    .ok_or_else(|| invalid("subscription was not committed"))
            })
            .await
    }

    /// Cancel owned observation even if access to its source has since been revoked.
    pub async fn unwatch_answers(&self, id: Uuid) -> io::Result<CommitReceipt> {
        self.authenticate()?;
        let client = self.clone();
        self.host
            .flow(self.source())
            .await?
            .request(move |store| {
                client.authenticate()?;
                store.unsubscribe_answers(id)
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
        // Authenticate before consulting the durable intent. Matching retries retain their receipt;
        // only a new intent and a new receiver admission require current send authority.
        self.authenticate()?;
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

fn existing_subscription(
    store: &crate::flow_store::FlowStore,
    requested: &AnswerSubscription,
) -> io::Result<Option<SubscriptionRecord>> {
    let existing = store
        .answer_subscriptions()
        .find(|record| record.subscription.id == requested.id);
    match existing {
        Some(record) if record.subscription != *requested => Err(invalid(
            "subscription identity conflicts with recorded registration",
        )),
        other => Ok(other.cloned()),
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
