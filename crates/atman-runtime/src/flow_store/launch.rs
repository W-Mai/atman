//! Atomic instance activation with its initial invocation and committed reply route.

use std::io;

use atman_rt::{FlowInstance, InstanceState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{Acceptance, AcceptedEnvelope, DeliveryRequest, Inbox, InboxPayload};
use super::subscription::{
    AnswerFilter, ResultRegistration, ResultSubscriptionProof, SubscriptionPurpose,
    SubscriptionStatus,
};
use super::{FlowDescriptor, FlowFact, FlowRef, FlowStore, invalid};
use crate::{event::AtmanUuid, event_log::journal::CommitReceipt, message::Message};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResultRoute {
    pub recipient: FlowRef,
    pub subscription: Uuid,
    pub synchronous_turn: Option<crate::event::TurnId>,
}

impl ResultRoute {
    pub fn filter(&self) -> AnswerFilter {
        self.synchronous_turn
            .clone()
            .map(AnswerFilter::ExceptTurn)
            .unwrap_or(AnswerFilter::All)
    }
}

/// Trusted host input, separate from the authority to execute it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LaunchInput {
    pub request: DeliveryRequest,
    pub inherited_context: Vec<Message>,
    pub result_route: Option<ResultRoute>,
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
    fn envelope(&self) -> AcceptedEnvelope {
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
        let receipt = self
            .journal
            .commit(&[FlowFact::Activated(Box::new(activation.clone()))])?;
        self.state = candidate;
        self.inbox.state_changed(&self.state, &receipt);
        let acceptance = activation.apply(&mut self.inbox, &receipt);
        self.cursor = receipt;
        let input = activation.input.clone();
        self.activation = Some(activation);
        self.refresh_projection();
        Ok(LaunchRecord { input, acceptance })
    }
}
