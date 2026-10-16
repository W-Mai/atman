//! Atomic instance activation with its initial invocation and committed reply route.

use std::io;

use atman_rt::{FlowInstance, InstanceState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{Acceptance, AcceptedEnvelope, DeliveryRequest, Inbox, InboxPayload};
use super::subscription::{
    FlowEventFilter, FlowSubscription, ResultRegistration, ResultSubscriptionProof,
    SubscriptionPurpose, SubscriptionStatus,
};
use super::{FlowDescriptor, FlowFact, FlowRef, FlowStore, invalid};
use crate::{event::AtmanUuid, event_log::journal::CommitReceipt, message::Message};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResultRoute {
    pub recipient: FlowRef,
    pub subscription: Uuid,
    pub synchronous: Option<SynchronousReply>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SynchronousReply {
    pub turn: crate::event::TurnId,
    pub subscription: Uuid,
    pub waiter: super::waiter::ReplyWaiter,
}

impl ResultRoute {
    pub fn filter(&self) -> FlowEventFilter {
        self.synchronous
            .as_ref()
            .map(|reply| FlowEventFilter::AnswersExceptTurn(reply.turn.clone()))
            .unwrap_or(FlowEventFilter::FinalAnswers)
    }

    pub fn completion_subscription(&self, source: &FlowRef) -> Option<FlowSubscription> {
        self.synchronous.as_ref().map(|reply| FlowSubscription {
            id: reply.subscription,
            source: source.clone(),
            filter: FlowEventFilter::TurnCompleted {
                turn: Some(reply.turn.clone()),
            },
            once: true,
            purpose: SubscriptionPurpose::SpawnResult,
            expires_at: None,
            reply: Some(reply.waiter.clone()),
        })
    }
}

/// Trusted host input, separate from the authority to execute it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LaunchInput {
    pub request: DeliveryRequest,
    /// Explicit initial objective, committed with activation before any turn can start.
    pub initial_goal: Option<String>,
    pub inherited_context: Vec<Message>,
    pub result_route: Option<ResultRoute>,
    pub execution: Option<EntryBinding>,
}

/// Recorded identity and structural ceilings; restoring them does not authorize execution.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EntryBinding {
    pub first_turn: crate::event::TurnId,
    pub first_run: crate::event::FlowRunId,
    pub authority: crate::flow_authority::EffectiveAuthority,
}

#[derive(Clone, Debug)]
pub struct LaunchRecord {
    pub input: LaunchInput,
    pub acceptance: Acceptance,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Activation {
    pub input: LaunchInput,
    id: Uuid,
    accepted_at: DateTime<Utc>,
    registration: Option<ResultRegistration>,
}

impl Activation {
    pub(super) fn goal_update(&self) -> Option<super::goal::GoalUpdate> {
        self.input
            .initial_goal
            .as_ref()
            .map(|text| super::goal::GoalUpdate {
                request: super::goal::GoalRequest {
                    id: self.input.request.request_id,
                    sender: self.input.request.sender.clone(),
                    expected: None,
                    edit: crate::memory::goal::GoalEdit::Create { text: text.clone() },
                },
                new_goal_id: self.id,
                at: self.accepted_at,
            })
    }

    pub(super) fn envelope(&self) -> AcceptedEnvelope {
        AcceptedEnvelope {
            id: self.id,
            accepted_at: self.accepted_at,
            request: self.input.request.clone(),
        }
    }

    pub fn validate(
        &self,
        descriptor: &FlowDescriptor,
        state: &FlowInstance<AtmanUuid>,
        inbox: &Inbox,
        cursor: &CommitReceipt,
    ) -> io::Result<FlowInstance<AtmanUuid>> {
        if state.state() != &InstanceState::Prepared
            || self.id.is_nil()
            || self.input.request.recipient != descriptor.address
            || self.input.request.request_id != descriptor.creation_request
            || self.input.request.scope.is_some()
            || self.input.request.cause.is_some()
            || !matches!(self.input.request.payload, InboxPayload::Invocation { .. })
        {
            return Err(invalid(
                "activation requires the prepared instance's original invocation",
            ));
        }
        if let Some(binding) = &self.input.execution
            && (binding.first_turn.0.0.is_nil()
                || binding.first_run.0.0.is_nil()
                || self
                    .input
                    .result_route
                    .as_ref()
                    .and_then(|route| route.synchronous.as_ref())
                    .is_some_and(|reply| reply.turn != binding.first_turn))
        {
            return Err(invalid(
                "entry binding has an invalid initial invocation identity",
            ));
        }
        match (&self.input.result_route, &self.registration) {
            (None, None) => {}
            (Some(route), Some(proof))
                if route.recipient == proof.recipient
                    && route.subscription == proof.record.subscription.id
                    && proof.record.subscription.source == descriptor.address
                    && proof.record.subscription.filter == route.filter()
                    && proof.record.subscription.purpose == SubscriptionPurpose::SpawnResult
                    && !proof.record.subscription.once
                    && proof.record.status == SubscriptionStatus::Active
                    && proof.record.subscription.reply.is_none()
                    && match (&route.synchronous, &proof.completion) {
                        (None, None) => true,
                        (Some(reply), Some(completion)) => {
                            reply.waiter.lease.flow == route.recipient
                                && reply.waiter.sender
                                    == super::inbox::SenderIdentity::Flow(
                                        descriptor.address.clone(),
                                    )
                                && Some(&completion.subscription)
                                    == route.completion_subscription(&descriptor.address).as_ref()
                                && completion.status == SubscriptionStatus::Active
                                && completion.started_after == proof.record.started_after
                                && completion.cursor == completion.started_after.sequence
                        }
                        _ => false,
                    }
                    && proof.record.started_after.journal_id
                        == descriptor.address.instance_id.0.0
                    && proof.record.started_after.sequence <= cursor.sequence
                    && proof.record.cursor == proof.record.started_after.sequence
                    && proof.receipt.journal_id == route.recipient.instance_id.0.0
                    && proof.receipt.sequence > 0 => {}
            _ => {
                return Err(invalid(
                    "activation has no matching committed result subscription",
                ));
            }
        }
        inbox.validate(&self.envelope())?;
        let mut candidate = state.clone();
        candidate.activate().map_err(invalid)?;
        Ok(candidate)
    }

    pub fn apply(&self, inbox: &mut Inbox, receipt: &CommitReceipt) -> Acceptance {
        inbox.apply(self.envelope(), receipt.clone())
    }
}

impl FlowStore {
    pub fn launch_record(&self) -> Option<LaunchRecord> {
        self.activation.as_ref().map(|activation| LaunchRecord {
            input: activation.input.clone(),
            acceptance: self
                .inbox
                .previous(&activation.input.request)
                .expect("validated launch input")
                .expect("committed launch acceptance"),
        })
    }

    /// Authenticate before calling. Identical retries recover acceptance without changing lifecycle,
    /// pause state, or consumption; only new activation requires current authorization and proof.
    pub fn activate_with_input(
        &mut self,
        input: LaunchInput,
        proof: Option<ResultSubscriptionProof>,
        authorize_new: impl FnOnce(&LaunchInput) -> io::Result<()>,
    ) -> io::Result<LaunchRecord> {
        if let Some(previous) = self.launch_record() {
            return if previous.input == input {
                Ok(previous)
            } else {
                Err(invalid(
                    "launch input conflicts with the committed activation",
                ))
            };
        }
        let activation = Activation {
            input,
            id: Uuid::now_v7(),
            accepted_at: Utc::now(),
            registration: proof.map(|p| p.0),
        };
        let candidate =
            activation.validate(&self.descriptor, &self.state, &self.inbox, &self.cursor)?;
        authorize_new(&activation.input)?;
        let goal = activation
            .goal_update()
            .map(|update| {
                self.goals
                    .prepare(&candidate, &update)
                    .map(|goal| (update, goal))
            })
            .transpose()?;
        let mut facts = vec![FlowFact::Activated(Box::new(activation.clone()))];
        if let Some((update, _)) = &goal {
            facts.push(FlowFact::GoalUpdated(Box::new(update.clone())));
        }
        let receipt = self.journal.commit(&facts)?;
        self.state = candidate;
        self.inbox.state_changed(&self.state, &receipt);
        let acceptance = activation.apply(&mut self.inbox, &receipt);
        if let Some((update, goal)) = goal {
            self.goals.apply(update, goal, receipt.clone());
        }
        self.cursor = receipt;
        let input = activation.input.clone();
        self.activation = Some(activation);
        self.refresh_projection();
        Ok(LaunchRecord { input, acceptance })
    }
}
