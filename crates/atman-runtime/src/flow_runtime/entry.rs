//! Shared host adapter for mapping pending input to a retained entry invocation.

use super::{FlowSnapshot, execution::TurnRequest};
use crate::{
    event::{FlowRunId, TurnId},
    flow_store::inbox::InboxPayload,
    message::Message,
};
use std::{collections::BTreeMap, io};
use uuid::Uuid;

/// The last durably admitted entry input, excluding conversation text.
#[derive(Clone, Debug)]
pub struct AdmittedInvocation {
    pub lease: atman_rt::TurnLease<crate::event::AtmanUuid>,
    pub arguments: BTreeMap<String, serde_json::Value>,
    pub invocation: BTreeMap<String, serde_json::Value>,
}

/// Immutable entry defaults and authority supplied by the trusted host.
/// Source execution still validates argument types and the instance lease at admission.
pub struct EntryRecipe {
    pub creation: Uuid,
    pub first_turn: TurnId,
    pub first_run: FlowRunId,
    pub arguments: BTreeMap<String, serde_json::Value>,
    pub invocation: BTreeMap<String, serde_json::Value>,
    pub parameter: Option<String>,
    pub authority: crate::flow_authority::EffectiveAuthority,
    pub yield_interval: std::num::NonZeroUsize,
}

impl EntryRecipe {
    pub fn prepare(&self, snapshot: &FlowSnapshot) -> io::Result<Option<TurnRequest>> {
        let eligible = |message: &&crate::flow_store::inbox::AcceptedEnvelope| {
            message.request.scope.is_none()
                && (matches!(
                    message.request.payload,
                    InboxPayload::Invocation { .. } | InboxPayload::GoalContinuation { .. }
                ) || self.parameter.is_some())
        };
        let ordinary = |message: &&crate::flow_store::inbox::AcceptedEnvelope| {
            eligible(message)
                && !matches!(
                    message.request.payload,
                    InboxPayload::GoalContinuation { .. }
                )
        };
        let Some(first) = snapshot
            .pending
            .iter()
            .find(ordinary)
            .or_else(|| snapshot.pending.iter().find(eligible))
        else {
            return Ok(None);
        };
        let initial = first.request.request_id == self.creation;
        let turn = if initial {
            self.first_turn.clone()
        } else {
            TurnId::now()
        };
        let run = if initial {
            self.first_run.clone()
        } else {
            FlowRunId::now()
        };
        let mut context = Vec::new();
        let mut messages = Vec::new();
        let (mut arguments, invocation) = match &first.request.payload {
            InboxPayload::GoalContinuation { source, .. } => {
                let admitted = snapshot
                    .last_invocation
                    .as_ref()
                    .filter(|admitted| &admitted.lease == source)
                    .ok_or_else(|| {
                        crate::flow_store::invalid("continuation source input is unavailable")
                    })?;
                messages.push(first.id);
                context.push(crate::flow_runtime::input::context_message(first, &turn)?);
                (admitted.arguments.clone(), admitted.invocation.clone())
            }
            InboxPayload::Invocation {
                arguments,
                invocation,
            } => {
                messages.push(first.id);
                context.push(crate::flow_runtime::input::context_message(first, &turn)?);
                (arguments.clone(), invocation.clone())
            }
            _ => {
                let mut invocation = self.invocation.clone();
                if let InboxPayload::UserSubmission {
                    invocation: requested,
                    ..
                } = &first.request.payload
                {
                    invocation.extend(requested.clone());
                }
                for message in snapshot.pending.iter().filter(ordinary) {
                    if matches!(message.request.payload, InboxPayload::Invocation { .. }) {
                        break;
                    }
                    if let InboxPayload::UserSubmission {
                        invocation: requested,
                        ..
                    } = &message.request.payload
                        && requested
                            .iter()
                            .any(|(key, value)| invocation.get(key) != Some(value))
                    {
                        break;
                    }
                    messages.push(message.id);
                    context.push(crate::flow_runtime::input::context_message(message, &turn)?);
                }
                (self.arguments.clone(), invocation)
            }
        };
        if !matches!(first.request.payload, InboxPayload::Invocation { .. })
            && let Some(parameter) = &self.parameter
        {
            arguments.insert(
                parameter.clone(),
                context
                    .iter()
                    .map(Message::text_concat)
                    .collect::<Vec<_>>()
                    .join("\n\n")
                    .into(),
            );
        }
        Ok(Some(TurnRequest {
            turn,
            run,
            messages,
            arguments,
            invocation,
            context,
            authority: self.authority.clone(),
            yield_interval: self.yield_interval,
        }))
    }
}
