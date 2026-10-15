//! Product invocation policy for host-owned persistent flow creation.

use std::{collections::BTreeMap, io};

use uuid::Uuid;

use super::*;
use crate::flow_runtime::{FlowSnapshot, execution::TurnRequest, launch::LaunchExecution};
use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity};
use crate::flow_store::launch::{LaunchInput, ResultRoute};
use crate::flow_store::{EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef};

pub(super) async fn spawn(
    args: ToolArgs,
    ctx: &ToolCtx,
    asynchronous: bool,
    catalog: Option<crate::event_log::journal::CommitReceipt>,
) -> ToolResult {
    let catalog = catalog.ok_or_else(|| {
        failed("inspect the persistent session with flow.instances before spawning")
    })?;
    let client = ctx
        .flow_client()
        .ok_or_else(|| failed("instance has no peer host binding"))?;
    if workspace_policy(&args)? != WorkspacePolicy::None {
        return Err(failed(
            "persistent workspace allocation requires an instance resource binding",
        ));
    }
    let reference = extract_flow(&args)?.unwrap_or_else(|| "subagent.at".into());
    let version = extract_flow_version(&args)?;
    let prepared = prepare_flow_agent(&reference, version.as_deref(), ctx).await?;
    let flow_args = resolve_flow_arguments(&prepared.flow, &args)?;
    let prompt = invocation_user_message(&prepared.flow, &flow_args)?;
    let parameter = invocation_message_parameter(&prepared.flow)?.map(str::to_owned);
    let arguments = flow_args
        .iter()
        .map(|(key, value)| Ok((key.clone(), value.to_json()?)))
        .collect::<Result<BTreeMap<_, _>, RuntimeError>>()?;
    let invocation = ctx
        .invocation_env
        .values()
        .map(|(key, value)| Ok((key.to_owned(), value.to_json()?)))
        .collect::<Result<BTreeMap<_, _>, RuntimeError>>()?;
    let identity = ctx
        .flow_identity
        .as_ref()
        .ok_or_else(|| failed("trusted identity is unavailable"))?;
    let authority = identity
        .effective_authority
        .inherited_child(
            crate::flow_authority::contract_allows_shell(prepared.flow.contract.as_ref()),
            crate::flow_authority::ChildWorkspaceAuthority::Inherit,
        )
        .map_err(failed)?;
    let yield_interval = ctx
        .vm_run_options
        .and_then(|options| options.yield_interval())
        .ok_or_else(|| failed("host has not selected a cooperative scheduling interval"))?;
    let creation = Uuid::now_v7();
    let first_turn = TurnId::now();
    let first_run = FlowRunId::now();
    let address = FlowRef {
        session_id: client.source().session_id,
        instance_id: FlowInstanceId::now(),
    };
    let inherited = should_inherit_context(&args);
    let mut history = if inherited {
        ctx.session_messages_handle
            .as_ref()
            .map(inherited_context_snapshot)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let handoff = handoff_spec(
        &prepared.flow,
        Some(identity.run_id.to_string()),
        &first_run,
        prompt.as_deref(),
        inherited,
    );
    for record in crate::context_plan::compile_context_records(&history, [handoff]) {
        history.push(Message::context_record(first_turn.clone(), record));
    }
    let descriptor = FlowDescriptor {
        address: address.clone(),
        creation_request: creation,
        created_by: Some(client.source().clone()),
        entry: EntrySnapshot {
            name: prepared.flow.name.name.clone(),
            path: prepared
                .program
                .entry_path()
                .ok_or_else(|| failed("entry source path is unavailable"))?
                .into(),
            bundle: prepared.program.revision_bundle(),
        },
    };
    let input = LaunchInput {
        request: DeliveryRequest {
            request_id: creation,
            sender: SenderIdentity::Flow(client.source().clone()),
            recipient: address.clone(),
            delivery: DeliveryMode::NextTurn,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::Invocation {
                arguments: arguments.clone(),
                invocation: invocation.clone(),
            },
        },
        inherited_context: history,
        result_route: Some(ResultRoute {
            recipient: client.source().clone(),
            subscription: Uuid::now_v7(),
            synchronous_turn: (!asynchronous).then(|| first_turn.clone()),
        }),
    };
    let recipe = Recipe {
        creation,
        first_turn: first_turn.clone(),
        first_run,
        arguments,
        invocation,
        parameter,
        authority,
        yield_interval,
    };
    let executor = executor(ctx)?;
    let (completed, observed) = tokio::sync::oneshot::channel();
    let mut completed = Some(completed);
    let launched = client
        .spawn(
            descriptor,
            input,
            LaunchExecution {
                executor,
                catalog,
                trust: ctx.trust.clone().unwrap_or_default(),
                prepare: Box::new(move |snapshot| recipe.prepare(snapshot)),
                completed: Box::new(move |completion| {
                    if completion.lease.turn == first_turn
                        && let Some(sender) = completed.take()
                    {
                        let result = match (completion.outcome, completion.result) {
                            (atman_rt::TurnOutcome::Completed, result) => result,
                            (_, Err(error)) => Err(error),
                            (_, Ok(_)) => Err(RuntimeError::Cancelled(
                                "instance turn was interrupted".into(),
                            )),
                        };
                        let _ = sender.send(result);
                    }
                }),
            },
        )
        .await
        .map_err(failed)?;
    if asynchronous {
        return Ok(Value::from_json(serde_json::json!({
            "handle": address.instance_id.to_string(), "address": address,
            "request_id": creation, "acceptance": launched.record.acceptance,
        })));
    }
    observed
        .await
        .map_err(|_| failed("instance scheduling ended before its initial turn returned"))?
}

/// Templates and shared services survive; per-invocation state belongs to the new worker.
fn executor(parent: &ToolCtx) -> Result<crate::Executor, RuntimeError> {
    let mut context = sanitize_child_ctx(parent);
    context.history_segment = crate::tool::HistorySegment::Root;
    context.deferred_input_session = None;
    context.session_messages = None;
    context.invocation_authorization = None;
    context.flow_identity = None;
    context.flow_run_id = None;
    context.turn_id = None;
    context.current_node_id = None;
    context.event_seq = None;
    context.agent_entry = None;
    context.goal_control = None;
    context.goal_resume_allowed = false;
    context.result_route = None;
    context.child_results = None;
    context.watch_hub = None;
    context.stream_tx = None;
    context.call_intent = None;
    context.tool_use_id = None;
    context.model_tool_exposures = None;
    context.model_mcp_scope = None;
    context.invocation_env = Default::default();
    context.flow_registry = None;
    context.permission_broker = None;
    context.approval = None;
    context.read_files = None;
    context.history_store = None;
    context.session_dir = None;
    context.current_model = None;
    context.cancel = Default::default();
    Ok(crate::Executor {
        tools: parent
            .registry
            .as_ref()
            .ok_or_else(|| failed("tool registry is unavailable"))?
            .as_ref()
            .clone(),
        providers: parent
            .providers
            .as_ref()
            .ok_or_else(|| failed("provider registry is unavailable"))?
            .as_ref()
            .clone(),
        events: parent.events.clone().unwrap_or_default(),
        safety: parent.safety.clone(),
        source_dir: None,
        tool_ctx: context,
    })
}

struct Recipe {
    creation: Uuid,
    first_turn: TurnId,
    first_run: FlowRunId,
    arguments: BTreeMap<String, serde_json::Value>,
    invocation: BTreeMap<String, serde_json::Value>,
    parameter: Option<String>,
    authority: crate::flow_authority::EffectiveAuthority,
    yield_interval: std::num::NonZeroUsize,
}

impl Recipe {
    fn prepare(&self, snapshot: &FlowSnapshot) -> io::Result<Option<TurnRequest>> {
        let eligible = |message: &&crate::flow_store::inbox::AcceptedEnvelope| {
            message.request.scope.is_none()
                && message.request.correlation.is_none()
                && (matches!(message.request.payload, InboxPayload::Invocation { .. })
                    || self.parameter.is_some())
        };
        let Some(first) = snapshot.pending.iter().find(eligible) else {
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
                for message in snapshot.pending.iter().filter(eligible) {
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
        if !matches!(first.request.payload, InboxPayload::Invocation { .. }) {
            arguments.insert(
                self.parameter
                    .clone()
                    .expect("validated text input mapping"),
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

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("flow.spawn: {error}"))
}
