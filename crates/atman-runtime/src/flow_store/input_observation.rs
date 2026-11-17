//! Body-free request consumption facts shared by live and recorded readers.

use super::{
    FlowInstanceId, FlowStore, SessionInspection, SessionProjection, SessionStore,
    inbox::SenderIdentity, invalid,
};
use crate::{event::AtmanUuid, flow_store::position::FlowPosition};
use atman_rt::TurnLease;
use serde::{Deserialize, Serialize};
use std::io;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InputObservation {
    pub message_id: Uuid,
    pub accepted: FlowPosition,
    pub input: InputDisposition,
    pub observed_receipt: FlowPosition,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InputDisposition {
    Pending,
    Cancelled {
        receipt: FlowPosition,
    },
    Committed {
        lease: TurnLease<AtmanUuid>,
        receipt: FlowPosition,
    },
}

impl FlowStore {
    /// Reconcile trusted local namespaces within this exact owner snapshot.
    pub fn observe_operator_input(
        &self,
        operator: super::local_operator::LocalOperator,
        request_id: Uuid,
    ) -> io::Result<Option<InputObservation>> {
        let sender = self.operator_input_sender(operator, None, request_id)?;
        self.observe_input_request(&sender, request_id)
    }

    pub(crate) fn operator_input_sender(
        &self,
        operator: super::local_operator::LocalOperator,
        requested: Option<&SenderIdentity>,
        request_id: Uuid,
    ) -> io::Result<SenderIdentity> {
        operator.resolve(requested, |sender| {
            self.inbox
                .accepted_request_head(sender, request_id)
                .is_some()
        })
    }

    /// The host authenticates sender; request identifiers never grant access to another actor's input.
    pub fn observe_input_request(
        &self,
        sender: &SenderIdentity,
        request_id: Uuid,
    ) -> io::Result<Option<InputObservation>> {
        self.inbox.observe_request(sender, request_id, &self.cursor)
    }
}

fn recorded(
    dir: &std::path::Path,
    catalog: &SessionProjection,
    instance: &FlowInstanceId,
    sender: &SenderIdentity,
    request_id: Uuid,
) -> io::Result<Option<InputObservation>> {
    let read = super::read_registered_flow(dir, catalog, instance, |_, _| Ok(()))?;
    let (_, _, inbox) = read
        .replay
        .projection
        .as_ref()
        .ok_or_else(|| invalid("flow creation is missing"))?;
    inbox.observe_request(
        sender,
        request_id,
        &read
            .journal
            .cursor
            .as_ref()
            .ok_or_else(|| invalid("flow journal has no cursor"))?
            .clone()
            .into(),
    )
}

impl SessionStore {
    pub(crate) fn inspect_operator_input(
        &self,
        instance: &FlowInstanceId,
        operator: super::local_operator::LocalOperator,
        request_id: Uuid,
    ) -> io::Result<Option<InputObservation>> {
        recorded_operator(&self.dir, &self.projection, instance, operator, request_id)
    }

    pub(crate) fn inspect_input_request(
        &self,
        instance: &FlowInstanceId,
        sender: &SenderIdentity,
        request_id: Uuid,
    ) -> io::Result<Option<InputObservation>> {
        recorded(&self.dir, &self.projection, instance, sender, request_id)
    }
}

impl SessionInspection {
    /// Reconcile an authenticated exact delivery without creating or recovering a writer.
    pub(crate) fn inspect_delivery(
        &self,
        request: &super::inbox::DeliveryRequest,
    ) -> io::Result<Option<super::inbox::Acceptance>> {
        if request.recipient.session_id.0 != self.projection.descriptor.id {
            return Err(invalid("delivery belongs to another recorded Session"));
        }
        let read = super::read_registered_flow(
            &self.dir,
            &self.projection,
            &request.recipient.instance_id,
            |_, _| Ok(()),
        )?;
        let (_, _, inbox) = read
            .replay
            .projection
            .as_ref()
            .ok_or_else(|| invalid("recorded delivery creation is unavailable"))?;
        inbox.previous(request)
    }
    /// Observe both retained local namespaces from one immutable replay.
    pub fn observe_operator_input(
        &self,
        instance: &FlowInstanceId,
        operator: super::local_operator::LocalOperator,
        request_id: Uuid,
    ) -> io::Result<Option<InputObservation>> {
        recorded_operator(&self.dir, &self.projection, instance, operator, request_id)
    }

    pub(crate) fn inspect_input_request(
        &self,
        instance: &FlowInstanceId,
        sender: &SenderIdentity,
        request_id: Uuid,
    ) -> io::Result<Option<InputObservation>> {
        recorded(&self.dir, &self.projection, instance, sender, request_id)
    }
}

fn recorded_operator(
    dir: &std::path::Path,
    catalog: &SessionProjection,
    instance: &FlowInstanceId,
    operator: super::local_operator::LocalOperator,
    request_id: Uuid,
) -> io::Result<Option<InputObservation>> {
    let read = super::read_registered_flow(dir, catalog, instance, |_, _| Ok(()))?;
    let (_, _, inbox) = read
        .replay
        .projection
        .as_ref()
        .ok_or_else(|| invalid("flow creation is missing"))?;
    let sender = operator.resolve(None, |sender| {
        inbox.accepted_request_head(sender, request_id).is_some()
    })?;
    let observed = read
        .journal
        .cursor
        .as_ref()
        .ok_or_else(|| invalid("flow journal has no cursor"))?
        .clone()
        .into();
    inbox.observe_request(&sender, request_id, &observed)
}
