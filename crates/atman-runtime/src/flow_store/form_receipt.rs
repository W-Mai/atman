//! Reconcile an operator's original deferred reply using its accepted inbox entry.

use std::io;

use super::{
    FlowFact, FlowRef, FlowStore, SessionInspection,
    inbox::{Inbox, InboxPayload, SenderIdentity},
    invalid,
    position::{FlowBatch, FlowPosition},
    read_registered_flow,
};
use crate::{
    flow_runtime::context::form_delivery_id,
    form::{DeferredFormReceipt, FormReplyLookup},
};

impl FlowStore {
    pub(crate) fn observe_form_reply(
        &mut self,
        lookup: &FormReplyLookup,
    ) -> io::Result<Option<DeferredFormReceipt>> {
        received(
            &self.inbox,
            &self.descriptor.address,
            &self.cursor,
            lookup,
            |receipt| self.journal.read_owned(receipt),
        )
    }
}

impl SessionInspection {
    pub(crate) fn observe_form_reply(
        &self,
        flow: &FlowRef,
        lookup: &FormReplyLookup,
    ) -> io::Result<Option<DeferredFormReceipt>> {
        if self.projection.descriptor.id != flow.session_id.0 {
            return Err(invalid("form reply belongs to another Session"));
        }
        let mut read = read_registered_flow(
            &self.dir,
            &self.projection,
            &flow.instance_id,
            |_, _| Ok(()),
        )?;
        let (_, _, inbox) = read
            .replay
            .projection
            .as_ref()
            .ok_or_else(|| invalid("recorded Flow creation is unavailable"))?;
        let cursor = read
            .journal
            .cursor
            .ok_or_else(|| invalid("Flow journal has no cursor"))?
            .into();
        received(inbox, flow, &cursor, lookup, |receipt| {
            read.reader
                .read_batch(receipt.require_durable()?)
                .map(Into::into)
        })
    }
}

fn received(
    inbox: &Inbox,
    flow: &FlowRef,
    cursor: &FlowPosition,
    lookup: &FormReplyLookup,
    read: impl FnMut(&FlowPosition) -> io::Result<FlowBatch<FlowFact>>,
) -> io::Result<Option<DeferredFormReceipt>> {
    let prompt = lookup.prompt_id.to_string();
    let sender = SenderIdentity::Host(flow.instance_id.0.0);
    let request = form_delivery_id(flow, &prompt);
    let Some(observed) = inbox.observe_request(&sender, request, cursor)? else {
        return Ok(None);
    };
    let entry = inbox
        .read_entry(observed.message_id, read)?
        .ok_or_else(|| invalid("accepted form reply body is unavailable"))?;
    let InboxPayload::FormReply { answer } = entry.envelope.request.payload else {
        return Err(invalid("form reply receipt belongs to another input kind"));
    };
    if answer.prompt_id != prompt || entry.envelope.request.recipient != *flow {
        return Err(invalid("form reply receipt has another source"));
    }
    let operation = answer.operation.filter(|operation| {
        operation.request_id == lookup.request_id
            && operation.host_generation == lookup.host_generation
    });
    Ok(operation
        .filter(|_| answer.expected_head.as_ref() == Some(&lookup.head))
        .map(|operation| DeferredFormReceipt {
            operation,
            acceptance: entry.acceptance,
        }))
}
