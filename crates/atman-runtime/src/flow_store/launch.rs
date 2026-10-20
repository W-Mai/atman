//! Atomic entry activation with optional initial input and a committed reply route.

use std::{collections::BTreeMap, io};

use atman_rt::{FlowInstance, InstanceState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{
    Acceptance, AcceptedEnvelope, DeliveryMode, DeliveryRequest, Inbox, InboxPayload,
    SenderIdentity, UserContent,
};
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
    /// Explicit lifetime membership, bound while the instance is still Prepared.
    pub scope: Option<super::task_scope::TaskScopeRef>,
    pub workspace: Option<super::workspace::WorkspaceRequest>,
    pub request: EntryRequest,
    /// Explicit initial objective, committed with activation before any turn can start.
    pub initial_goal: Option<String>,
    pub inherited_context: Vec<Message>,
    pub result_route: Option<ResultRoute>,
    pub execution: Option<EntryBinding>,
}

impl LaunchInput {
    pub(crate) fn validate(&self) -> io::Result<()> {
        self.request.validate_source()?;
        if let Some(scope) = &self.scope {
            scope.validate()?;
        }
        if matches!(self.request.start, EntryStart::Waiting)
            && (self.execution.is_none()
                || self
                    .result_route
                    .as_ref()
                    .is_some_and(|route| route.synchronous.is_some()))
        {
            return Err(invalid(
                "waiting entry requires an execution binding and cannot await an initial result",
            ));
        }
        Ok(())
    }
}

/// Creation identity and entry defaults exist independently of pending invocation input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EntryRequest {
    pub request_id: Uuid,
    pub sender: SenderIdentity,
    pub recipient: FlowRef,
    pub arguments: BTreeMap<String, serde_json::Value>,
    pub invocation: BTreeMap<String, serde_json::Value>,
    pub start: EntryStart,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryStart {
    Waiting,
    Invoke {
        user: Option<UserContent>,
    },
    /// A captured startup command; subsequent ordinary input uses the default entry.
    Command {
        entry: Box<super::EntrySnapshot>,
        arguments: BTreeMap<String, serde_json::Value>,
        invocation: BTreeMap<String, serde_json::Value>,
    },
}

impl EntryRequest {
    pub fn initial_input(&self) -> Option<DeliveryRequest> {
        let payload = match &self.start {
            EntryStart::Waiting => return None,
            EntryStart::Invoke { user } => InboxPayload::Invocation {
                arguments: self.arguments.clone(),
                invocation: self.invocation.clone(),
                user: user.clone(),
            },
            EntryStart::Command {
                entry,
                arguments,
                invocation,
            } => InboxPayload::Command {
                entry: entry.clone(),
                arguments: arguments.clone(),
                invocation: invocation.clone(),
                user: None,
            },
        };
        Some(DeliveryRequest {
            request_id: self.request_id,
            sender: self.sender.clone(),
            recipient: self.recipient.clone(),
            delivery: DeliveryMode::NextTurn,
            cause: None,
            correlation: None,
            scope: None,
            payload,
        })
    }

    pub(crate) fn validate_source(&self) -> io::Result<()> {
        if self.request_id.is_nil() {
            return Err(invalid("entry request identity is empty"));
        }
        if let Some(input) = self.initial_input() {
            input.validate_source()?;
        }
        Ok(())
    }
}

/// Recorded identity and structural ceilings; restoring them does not authorize execution.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EntryBinding {
    /// Interactive clients select human text through the bound configuration's routes.at.
    pub user_routes: bool,
    /// Execute container startup hooks in the first admitted invocation of its initial entry.
    pub session_start: bool,
    pub first_turn: crate::event::TurnId,
    pub first_run: crate::event::FlowRunId,
    pub authority: crate::flow_authority::EffectiveAuthority,
}

#[derive(Clone, Debug)]
pub struct LaunchRecord {
    pub input: LaunchInput,
    pub receipt: CommitReceipt,
    pub acceptance: Option<Acceptance>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Activation {
    pub input: LaunchInput,
    id: Uuid,
    pub(super) accepted_at: DateTime<Utc>,
    registration: Option<ResultRegistration>,
}

impl Activation {
    pub(super) fn goal_update(&self) -> Option<super::goal::GoalUpdate> {
        self.input
            .initial_goal
            .as_ref()
            .map(|text| super::goal::GoalUpdate {
                request: super::goal::GoalRequest {
                    start_turn: false,
                    id: self.input.request.request_id,
                    sender: self.input.request.sender.clone(),
                    expected: None,
                    edit: crate::memory::goal::GoalEdit::Create { text: text.clone() },
                },
                new_goal_id: self.id,
                at: self.accepted_at,
                input: None,
            })
    }

    pub(super) fn envelope(&self) -> Option<AcceptedEnvelope> {
        self.input
            .request
            .initial_input()
            .map(|request| AcceptedEnvelope {
                id: self.id,
                accepted_at: self.accepted_at,
                request,
            })
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
        {
            return Err(invalid(
                "activation requires the prepared instance's creation identity",
            ));
        }
        self.input.validate()?;
        if let Some(binding) = &self.input.execution
            && (binding.first_turn.0.0.is_nil()
                || binding.first_run.0.0.is_nil()
                || (binding.session_start
                    && (descriptor.created_by.is_some()
                        || matches!(
                            self.input.request.sender,
                            super::inbox::SenderIdentity::Flow(_)
                        )))
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
        if let Some(envelope) = self.envelope() {
            inbox.validate(&envelope)?;
        }
        let mut candidate = state.clone();
        candidate.activate().map_err(invalid)?;
        Ok(candidate)
    }

    pub fn apply(&self, inbox: &mut Inbox, receipt: &CommitReceipt) -> Option<Acceptance> {
        self.envelope()
            .map(|envelope| inbox.apply(envelope, receipt.clone()))
    }
}

impl FlowStore {
    pub fn launch_record(&self) -> Option<LaunchRecord> {
        self.activation
            .as_ref()
            .map(|(activation, receipt)| LaunchRecord {
                input: activation.input.clone(),
                receipt: receipt.clone(),
                acceptance: activation.input.request.initial_input().map(|request| {
                    self.inbox
                        .previous(&request)
                        .expect("validated launch input")
                        .expect("committed launch acceptance")
                }),
            })
    }

    /// Authenticate before calling. Identical retries recover activation and input receipts without changing lifecycle, pause state, or consumption; only new activation requires current authorization and proof.
    pub fn activate_entry(
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
        if input.scope.as_ref() != self.task_scope_binding() {
            return Err(invalid(
                "activation scope does not match its recorded membership",
            ));
        }
        if input.workspace != self.workspace {
            return Err(invalid(
                "activation workspace does not match its recorded request",
            ));
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
        self.cursor = receipt.clone();
        let input = activation.input.clone();
        self.activation = Some((activation, receipt.clone()));
        self.refresh_projection();
        Ok(LaunchRecord {
            input,
            receipt,
            acceptance,
        })
    }
}
