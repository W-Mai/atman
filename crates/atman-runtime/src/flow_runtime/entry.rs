//! Shared host adapter for mapping pending input to a retained entry invocation.

use super::{FlowSnapshot, execution::TurnRequest};
use crate::{
    event::{FlowRunId, TurnId},
    flow_authority::EffectiveAuthority,
    flow_store::inbox::InboxPayload,
    flow_store::{FlowDescriptor, invalid, launch::LaunchInput},
    message::Message,
};
use std::{collections::BTreeMap, io, num::NonZeroUsize};
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
    /// Rebind recorded policy under current host ceilings, without releasing execution gates.
    pub fn from_launch(
        session: &crate::flow_store::SessionDescriptor,
        descriptor: &FlowDescriptor,
        input: &LaunchInput,
        current: &EffectiveAuthority,
        yield_interval: NonZeroUsize,
    ) -> io::Result<Self> {
        if descriptor.address.session_id.0 != session.id
            || input.request.recipient != descriptor.address
            || input.request.request_id != descriptor.creation_request
        {
            return Err(invalid("entry input does not match instance creation"));
        }
        let binding = input
            .execution
            .as_ref()
            .ok_or_else(|| invalid("instance has no recorded entry binding"))?;
        let InboxPayload::Invocation {
            arguments,
            invocation,
        } = &input.request.payload
        else {
            return Err(invalid("entry binding requires invocation input"));
        };
        let program = crate::source_program::load_program_from_bundle(
            &descriptor.entry.bundle,
            &descriptor.entry.path,
            &session.source_roots(),
        )
        .map_err(invalid)?;
        let flow = program
            .entry_flow(&descriptor.entry.name)
            .and_then(|id| program.flow(&id))
            .ok_or_else(|| invalid("recorded entry is unavailable"))?;
        let parameter = crate::validate::invocation_message_parameter(flow)
            .map_err(invalid)?
            .map(str::to_owned);
        let authority = current
            .for_child(
                &binding.authority,
                crate::flow_authority::contract_allows_shell(flow.contract.as_ref()),
                binding.authority.workspace_root.clone(),
            )
            .map_err(invalid)?;
        Ok(Self {
            creation: descriptor.creation_request,
            first_turn: binding.first_turn.clone(),
            first_run: binding.first_run.clone(),
            arguments: arguments.clone(),
            invocation: invocation.clone(),
            parameter,
            authority,
            yield_interval,
        })
    }

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
        let initial =
            snapshot.last_invocation.is_none() && first.request.request_id == self.creation;
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

impl super::FlowWorker {
    /// Resolve the persisted entry under caller-supplied ceilings. This does not start scheduling.
    pub async fn entry_recipe(
        &self,
        current: EffectiveAuthority,
        yield_interval: NonZeroUsize,
    ) -> io::Result<EntryRecipe> {
        self.request(move |store| {
            let launch = store
                .launch_record()
                .ok_or_else(|| invalid("instance has no committed launch input"))?;
            EntryRecipe::from_launch(
                store.session_descriptor(),
                store.descriptor(),
                &launch.input,
                &current,
                yield_interval,
            )
        })
        .await
    }
}

impl super::host::SessionHandle {
    /// Explicitly attach the shared scheduler to a recorded entry.
    /// Current trust remains in the instance context; this does not restore old trust settings.
    pub async fn bind_entry(
        &self,
        id: crate::flow_store::FlowInstanceId,
        executor: crate::Executor,
        current: EffectiveAuthority,
        yield_interval: NonZeroUsize,
    ) -> io::Result<tokio::sync::watch::Receiver<super::scheduler::SchedulingStatus>> {
        let worker = self.open_flow(id.clone()).await?;
        let recipe = worker.entry_recipe(current, yield_interval).await?;
        self.schedule_flow(id, executor, move |snapshot| recipe.prepare(snapshot))
            .await
    }
}
