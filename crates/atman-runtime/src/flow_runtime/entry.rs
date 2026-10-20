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

/// JSON transports encode paths as strings; restore declared host paths before VM validation.
pub(super) fn decode_argument(
    value: serde_json::Value,
    ty: &atman_rt::ast::TypeExpr,
) -> crate::Value {
    use crate::{Value, ValueJson};
    use atman_rt::ast::TypeExpr;
    match (value, ty) {
        (serde_json::Value::String(path), TypeExpr::Named(name))
            if name.name.eq_ignore_ascii_case("path") =>
        {
            Value::Host(crate::AtmanPayload::Path(path.into()))
        }
        (serde_json::Value::Array(items), TypeExpr::List(item)) => Value::List(
            items
                .into_iter()
                .map(|value| decode_argument(value, item))
                .collect(),
        ),
        (serde_json::Value::Object(fields), TypeExpr::Struct(types)) => Value::Struct(
            fields
                .into_iter()
                .map(|(name, value)| {
                    let value = match types.iter().find(|(field, _)| field.name == name) {
                        Some((_, ty)) => decode_argument(value, ty),
                        None => Value::from_json(value),
                    };
                    (name, value)
                })
                .collect(),
        ),
        (value, _) => Value::from_json(value),
    }
}

/// The last durably admitted entry input, excluding conversation text.
#[derive(Clone, Debug)]
pub struct AdmittedInvocation {
    pub command: Option<crate::flow_store::EntrySnapshot>,
    pub lease: atman_rt::TurnLease<crate::event::AtmanUuid>,
    pub arguments: BTreeMap<String, serde_json::Value>,
    pub invocation: BTreeMap<String, serde_json::Value>,
}

/// Immutable entry defaults and authority supplied by the trusted host.
/// Source execution still validates argument types and the instance lease at admission.
pub struct EntryRecipe {
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
        project: Option<&crate::storage::ProjectContext>,
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
        input.validate()?;
        let arguments = &input.request.arguments;
        let invocation = &input.request.invocation;
        let entry = descriptor.entry()?;
        let program = crate::source_program::load_program_from_bundle(
            &entry.bundle,
            &entry.path,
            &session.source_roots_for(project),
        )
        .map_err(invalid)?;
        let flow = program
            .entry_flow(&entry.name)
            .and_then(|id| program.flow(&id))
            .ok_or_else(|| invalid("recorded entry is unavailable"))?;
        let parameter = crate::validate::invocation_message_parameter(flow)
            .map_err(invalid)?
            .map(str::to_owned);
        let shell = crate::flow_authority::contract_allows_shell(flow.contract.as_ref());
        let authority = match &input.workspace {
            Some(workspace) => workspace.constrain_authority(
                &descriptor.address,
                current,
                &binding.authority,
                shell,
            )?,
            None => current
                .for_child(
                    &binding.authority,
                    shell,
                    binding.authority.workspace_root.clone(),
                )
                .map_err(invalid)?,
        };
        Ok(Self {
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
                    InboxPayload::Invocation { .. }
                        | InboxPayload::Command { .. }
                        | InboxPayload::GoalContinuation { .. }
                        | InboxPayload::GoalResume { .. }
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
        let initial = snapshot.last_invocation.is_none();
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
            InboxPayload::GoalResume { .. } => {
                messages.push(first.id);
                context.push(crate::flow_runtime::input::context_message(first, &turn)?);
                snapshot
                    .last_invocation
                    .as_ref()
                    .map(|admitted| (admitted.arguments.clone(), admitted.invocation.clone()))
                    .unwrap_or_else(|| (self.arguments.clone(), self.invocation.clone()))
            }
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
            InboxPayload::Command {
                arguments,
                invocation,
                ..
            }
            | InboxPayload::Invocation {
                arguments,
                invocation,
                ..
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
                    if matches!(
                        message.request.payload,
                        InboxPayload::Invocation { .. }
                            | InboxPayload::Command { .. }
                            | InboxPayload::GoalResume { .. }
                    ) {
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
        let parameter = if matches!(
            first.request.payload,
            InboxPayload::GoalResume { .. } | InboxPayload::GoalContinuation { .. }
        ) {
            match snapshot
                .last_invocation
                .as_ref()
                .and_then(|invocation| invocation.command.as_ref())
            {
                Some(entry) => {
                    let source =
                        entry
                            .bundle
                            .sources
                            .get(&entry.bundle.entry)
                            .ok_or_else(|| {
                                crate::flow_store::invalid("command source is unavailable")
                            })?;
                    let file = atman_rt::parse_file(source).map_err(crate::flow_store::invalid)?;
                    let flow = file
                        .flows
                        .iter()
                        .find(|flow| flow.name.name == entry.name)
                        .ok_or_else(|| {
                            crate::flow_store::invalid("command entry is unavailable")
                        })?;
                    crate::validate::invocation_message_parameter(flow)
                        .map_err(crate::flow_store::invalid)?
                        .map(str::to_owned)
                }
                None => self.parameter.clone(),
            }
        } else {
            self.parameter.clone()
        };
        if !matches!(
            first.request.payload,
            InboxPayload::Invocation { .. } | InboxPayload::Command { .. }
        ) && let Some(parameter) = &parameter
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
            if let Some(workspace) = store.workspace_request() {
                workspace.validate_live(store.state().address())?;
            }
            EntryRecipe::from_launch(
                store.session_descriptor(),
                store.descriptor(),
                &launch.input,
                &current,
                yield_interval,
                store.project(),
            )
        })
        .await
    }
}

impl super::host::SessionHandle {
    /// Explicitly attach the shared scheduler to a recorded entry.
    /// Apply current host trust without releasing execution gates or replaying consumed input. A live scheduler cannot be replaced or have its trust changed by a competing binding.
    pub async fn bind_entry(
        &self,
        id: crate::flow_store::FlowInstanceId,
        executor: crate::Executor,
        current: EffectiveAuthority,
        trust: crate::trust::TrustConfig,
        yield_interval: NonZeroUsize,
    ) -> io::Result<tokio::sync::watch::Receiver<super::scheduler::SchedulingStatus>> {
        let worker = self.open_flow(id.clone()).await?;
        let _binding = worker.launch_gate.clone().lock_owned().await;
        if self
            .scheduling_status(id.clone())
            .await?
            .is_some_and(|status| *status.borrow() == super::scheduler::SchedulingStatus::Running)
        {
            return Err(invalid("instance already has a host scheduler"));
        }
        if !matches!(
            worker.state().await?.state(),
            atman_rt::InstanceState::Open(_)
        ) {
            return Err(invalid("only an open instance can bind entry scheduling"));
        }
        let recipe = worker.entry_recipe(current, yield_interval).await?;
        worker.context().bind_executor(&executor).map_err(invalid)?;
        worker
            .context()
            .update_trust(trust, |_| Ok(()))
            .map_err(invalid)?;
        self.install_scheduler(
            id,
            executor,
            Box::new(move |snapshot| recipe.prepare(snapshot)),
        )
        .await
    }
}
