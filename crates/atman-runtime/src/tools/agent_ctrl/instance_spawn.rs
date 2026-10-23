//! Product invocation policy for host-owned persistent flow creation.

use std::collections::BTreeMap;

use uuid::Uuid;

use super::*;
use crate::flow_runtime::launch::EntryExecution;
use crate::flow_store::inbox::SenderIdentity;
use crate::flow_store::launch::{EntryBinding, LaunchInput, ResultRoute, SynchronousReply};
use crate::flow_store::{EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef};

pub(super) async fn spawn(
    args: ToolArgs,
    ctx: &ToolCtx,
    asynchronous: bool,
    catalog: Option<crate::event_log::journal::CommitReceipt>,
) -> ToolResult {
    let scope = crate::tools::task_scope::reference(&args)?;
    let catalog = catalog.ok_or_else(|| {
        failed("inspect the persistent session with flow.instances before spawning")
    })?;
    let client = ctx
        .flow_client()
        .ok_or_else(|| failed("instance has no peer host binding"))?;
    let executor = client
        .spawn_executor(&catalog, executor(ctx)?)
        .await
        .map_err(failed)?;
    let changed_project = ctx.project() != executor.tool_ctx.project();
    let source_ctx = if changed_project {
        let mut source = executor.tool_ctx.clone();
        source.providers = Some(Arc::new(executor.providers.clone()));
        source.registry = Some(Arc::new(executor.tools.clone()));
        source
    } else {
        ctx.clone()
    };
    let policy = workspace_policy(&args)?;
    let reference = extract_flow(&args)?.unwrap_or_else(|| "subagent.at".into());
    let version = extract_flow_version(&args)?;
    let prepared = prepare_flow_agent(&reference, version.as_deref(), &source_ctx).await?;
    let flow_args = resolve_flow_arguments(&prepared.flow, &args)?;
    let prompt = invocation_user_message(&prepared.flow, &flow_args)?;
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
    let mut creation_authority = identity.effective_authority.clone();
    if changed_project {
        // Membership is an operator-selected catalog fact, not a model-supplied workspace.
        // Capability ceilings remain inherited even when the new instance uses another project.
        creation_authority.workspace_root = executor
            .tool_ctx
            .project()
            .map(|project| project.root().to_owned());
    }
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
    let workspace = if policy == WorkspacePolicy::None {
        source_ctx
            .workspace
            .clone()
            .map(|binding| crate::flow_store::workspace::WorkspaceRequest {
                binding,
                source: crate::flow_store::workspace::WorkspaceSource::Share {
                    sponsor: client.source().clone(),
                },
            })
    } else {
        let service = source_ctx
            .flow_workspace_service
            .clone()
            .ok_or_else(|| failed("workspace service is unavailable"))?;
        let owner = address.clone();
        let root = creation_authority.workspace_root.clone();
        Some(
            tokio::task::spawn_blocking(move || {
                service.prepare_instance(policy, &owner, root.as_deref())
            })
            .await
            .map_err(failed)?
            .map_err(failed)?,
        )
    };
    let authority = creation_authority
        .inherited_child(
            crate::flow_authority::contract_allows_shell(prepared.flow.contract.as_ref()),
            workspace
                .as_ref()
                .map(|workspace| {
                    crate::flow_authority::ChildWorkspaceAuthority::TrustedDelegation(
                        workspace.binding.path.clone(),
                    )
                })
                .unwrap_or(crate::flow_authority::ChildWorkspaceAuthority::Inherit),
        )
        .map_err(failed)?;
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
    let inherited_outputs = ctx
        .output_store
        .as_deref()
        .cloned()
        .unwrap_or_default()
        .capture_references(&history)?;
    let descriptor = FlowDescriptor {
        address: address.clone(),
        creation_request: creation,
        created_by: Some(client.source().clone()),
        content: EntrySnapshot {
            name: prepared.flow.name.name.clone(),
            path: prepared
                .program
                .entry_path()
                .ok_or_else(|| failed("entry source path is unavailable"))?
                .into(),
            bundle: prepared.program.revision_bundle(),
        }
        .into(),
    };
    let reply = if asynchronous {
        None
    } else {
        Some(
            client
                .prepare_reply(address.clone(), creation)
                .await
                .map_err(failed)?,
        )
    };
    let input = LaunchInput {
        scope,
        workspace,
        initial_goal: None,
        execution: Some(EntryBinding {
            user_routes: false,
            session_start: false,
            first_turn: first_turn.clone(),
            first_run,
            authority: authority.clone(),
        }),
        request: crate::flow_store::launch::EntryRequest {
            request_id: creation,
            sender: SenderIdentity::Flow(client.source().clone()),
            recipient: address.clone(),
            arguments: arguments.clone(),
            invocation: invocation.clone(),
            start: crate::flow_store::launch::EntryStart::Invoke { user: None },
        },
        inherited_context: history,
        inherited_outputs,
        result_route: Some(ResultRoute {
            recipient: client.source().clone(),
            subscription: Uuid::now_v7(),
            synchronous: reply.as_ref().map(|reply| SynchronousReply {
                turn: first_turn.clone(),
                subscription: Uuid::now_v7(),
                waiter: reply.waiter().clone(),
            }),
        }),
    };
    let launched = client
        .spawn(
            descriptor,
            input,
            EntryExecution {
                executor,
                catalog,
                authority: creation_authority,
                yield_interval,
                trust: ctx.trust.clone().unwrap_or_default(),
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
    reply
        .expect("synchronous reply registered")
        .completion(first_turn)
        .await
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

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("flow.spawn: {error}"))
}
