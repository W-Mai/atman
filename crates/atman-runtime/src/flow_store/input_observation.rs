//! Body-free request consumption facts shared by live and recorded readers.

use super::{
    FlowInstanceId, FlowStore, SessionInspection, SessionProjection, SessionStore,
    inbox::SenderIdentity, invalid,
};
use crate::{event::AtmanUuid, flow_store::position::FlowPosition};
use atman_rt::TurnLease;
use serde::Serialize;
use std::io;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize)]
pub struct InputObservation {
    pub message_id: Uuid,
    pub accepted: FlowPosition,
    pub input: InputDisposition,
    pub observed_receipt: FlowPosition,
}

#[derive(Clone, Debug, Serialize)]
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
    pub(crate) fn inspect_input_request(
        &self,
        instance: &FlowInstanceId,
        sender: &SenderIdentity,
        request_id: Uuid,
    ) -> io::Result<Option<InputObservation>> {
        recorded(&self.dir, &self.projection, instance, sender, request_id)
    }
}
