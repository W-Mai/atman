use crate::approval::authorize_tool_invocation;
use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::{AtmanPayload, Value, ValueJson};

pub struct ExtractToolUses;

impl Tool for ExtractToolUses {
    fn name(&self) -> &str {
        "extract_tool_uses"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Pull the tool_use parts out of an assistant Message. Returns a list of \
             {id, name, input, intent?} structs suitable for dispatch_all.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"message": {"description": "Assistant Message value."}},
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = match args.named("message") {
                Some(v) => v,
                None => args.positional(0)?,
            };
            let m = match v {
                Value::Host(AtmanPayload::Message(m)) => m,
                Value::Str(_) => return Ok(Value::List(Vec::new())),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "message or string".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            let mut out = Vec::new();
            let final_answer_error = crate::tools::final_answer::validation_error(m);
            for part in &m.parts {
                if let crate::message::MessagePart::ToolUse {
                    id,
                    name,
                    input,
                    intent,
                } = part
                {
                    if name == crate::tools::final_answer::FINAL_ANSWER_TOOL
                        && final_answer_error.is_none()
                    {
                        continue;
                    }
                    let mut fields = vec![
                        ("id".into(), Value::Str(id.clone())),
                        ("name".into(), Value::Str(name.clone())),
                        ("input".into(), Value::from_json(input.clone())),
                    ];
                    if let Some(intent) = intent {
                        fields.push(("intent".into(), Value::Str(intent.as_str().into())));
                    }
                    if name == crate::tools::final_answer::FINAL_ANSWER_TOOL
                        && let Some(error) = final_answer_error
                    {
                        fields.push(("validation_error".into(), Value::Str(error.into())));
                    }
                    out.push(Value::Struct(fields));
                }
            }
            Ok(Value::List(out))
        })
    }
}

pub struct DispatchAll;

impl Tool for DispatchAll {
    fn name(&self) -> &str {
        "dispatch_all"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Dispatch each tool_use in the list against the current tool registry and \
             return a list of tool_result Message values.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"tool_uses": {"type": "array"}},
            "required": ["tool_uses"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let uses = extract_list(&args, "tool_uses", 0)?;
            let Some(registry) = ctx.registry.as_ref() else {
                return Err(RuntimeError::ToolFailed(
                    "dispatch_all: no tool registry available on ctx".into(),
                ));
            };
            let prepared = prepare_dispatch(&uses, registry.as_ref(), ctx)?;
            let out_slots = dispatch_prepared(prepared, ctx).await;
            let outcomes = out_slots
                .into_iter()
                .map(|outcome| outcome.expect("every dispatch slot must be settled"))
                .collect::<Vec<_>>();
            let mut out = Vec::with_capacity(
                outcomes.len()
                    + outcomes
                        .iter()
                        .map(|outcome| outcome.followups.len())
                        .sum::<usize>(),
            );
            out.extend(outcomes.iter().map(|outcome| outcome.tool_result.clone()));
            out.extend(outcomes.into_iter().flat_map(|outcome| outcome.followups));
            Ok(Value::List(out))
        })
    }
}

enum PreparedEntry {
    Ready(Box<PreparedCall>),
    Failed {
        index: usize,
        msg: crate::message::Message,
        plane: crate::tool::InvocationPlane,
    },
}

impl PreparedEntry {
    fn plane(&self) -> crate::tool::InvocationPlane {
        match self {
            Self::Ready(call) => call.plane,
            Self::Failed { plane, .. } => *plane,
        }
    }
}

struct PreparedCall {
    index: usize,
    id: String,
    name: String,
    tool: std::sync::Arc<dyn Tool>,
    call_args: ToolArgs,
    invocation_ctx: ToolCtx,
    claims: crate::tool_schedule::SchedulingClaims,
    plane: crate::tool::InvocationPlane,
}

fn exposure_error_message(
    error: &crate::tool::ToolExposureError,
    id: &str,
    name: &str,
    registry: &crate::tool::ToolRegistry,
) -> String {
    use crate::tool::ToolExposureError;
    match error {
        ToolExposureError::NotExposed {
            exposed_names,
            scope,
        } => {
            let mut message = format!(
                "tool_not_exposed: tool `{name}` was not executed because it was not exposed by its LLM request. Repeating this tool name will not fix the error."
            );
            if let Some(hint) =
                crate::tools::mcp::unexposed_tool_hint(name, registry, exposed_names, scope)
            {
                message.push(' ');
                message.push_str(&hint);
            } else {
                message.push_str(" Choose a tool from the current request's available tools.");
            }
            message
        }
        ToolExposureError::AmbiguousCallId => format!(
            "ambiguous_tool_call_id: tool `{name}` was not executed because call ID `{id}` is ambiguous across pending calls. Issue a fresh tool call with a unique ID; do not replay this call."
        ),
        ToolExposureError::MissingReceipt => format!(
            "missing_tool_call_receipt: tool `{name}` was not executed because no pending authorization receipt matches this flow, call ID `{id}`, and tool name. The receipt may already have been consumed. Issue a fresh call using the current request's available tools; do not replay this call."
        ),
    }
}

fn prepare_dispatch(
    uses: &[Value],
    registry: &crate::tool::ToolRegistry,
    ctx: &ToolCtx,
) -> Result<Vec<PreparedEntry>, RuntimeError> {
    let parsed = uses
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let Value::Struct(fields) = entry else {
                return Err(RuntimeError::TypeMismatch {
                    expected: "struct {id, name, input}".into(),
                    actual: entry.kind_name().into(),
                });
            };
            let get = |key: &str| {
                fields
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            };
            let id = match get("id") {
                Some(Value::Str(id)) => id,
                _ => {
                    return Err(RuntimeError::ToolFailed(
                        "dispatch_all: tool_use missing `id` string".into(),
                    ));
                }
            };
            let name = match get("name") {
                Some(Value::Str(name)) => name,
                _ => {
                    return Err(RuntimeError::ToolFailed(
                        "dispatch_all: tool_use missing `name` string".into(),
                    ));
                }
            };
            let call_intent = match get("intent") {
                Some(Value::Str(value)) => crate::message::ToolCallIntent::new(value),
                _ => None,
            };
            let validation_error = match get("validation_error") {
                Some(Value::Str(value)) => Some(value),
                _ => None,
            };
            Ok((
                index,
                id,
                name,
                get("input").unwrap_or(Value::Unit),
                call_intent,
                validation_error,
            ))
        })
        .collect::<Result<Vec<_>, RuntimeError>>()?;

    let mut prepared = Vec::with_capacity(parsed.len());
    for (index, id, name, input, mut call_intent, validation_error) in parsed {
        if let Some(error) = validation_error {
            emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
            prepared.push(PreparedEntry::Failed {
                index,
                plane: registry
                    .get(&name)
                    .map_or(crate::tool::InvocationPlane::Ordinary, |tool| {
                        tool.invocation_plane()
                    }),
                msg: build_error_result(
                    ctx,
                    &id,
                    &format!("tool `{name}` was not executed: {error}"),
                ),
            });
            continue;
        }
        let model_mcp_scope = match ctx.model_tool_exposures.as_ref() {
            Some(exposures) => exposures
                .claim_with_mcp_scope(ctx.flow_run_id.as_ref(), &id, &name)
                .map(Some),
            None => Ok(ctx.model_mcp_scope.clone()),
        };
        let model_mcp_scope = match model_mcp_scope {
            Ok(scope) => scope,
            Err(error) => {
                emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
                prepared.push(PreparedEntry::Failed {
                    index,
                    plane: registry
                        .get(&name)
                        .map_or(crate::tool::InvocationPlane::Ordinary, |tool| {
                            tool.invocation_plane()
                        }),
                    msg: build_error_result(
                        ctx,
                        &id,
                        &exposure_error_message(&error, &id, &name, registry),
                    ),
                });
                continue;
            }
        };
        let Some(tool) = registry.get(&name) else {
            emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
            prepared.push(PreparedEntry::Failed {
                index,
                plane: crate::tool::InvocationPlane::Ordinary,
                msg: build_error_result(ctx, &id, &format!("dispatch_all: unknown tool `{name}`")),
            });
            continue;
        };
        let plane = tool.invocation_plane();
        let raw_schema = tool.input_schema();
        let named = match &input {
            Value::Struct(fields) => {
                let mut fields = fields.clone();
                if !crate::tool::tool_schema_uses_call_intent_field(&raw_schema)
                    && let Some(index) = fields
                        .iter()
                        .position(|(name, _)| name == crate::message::TOOL_CALL_INTENT_FIELD)
                {
                    let (_, value) = fields.remove(index);
                    if call_intent.is_none()
                        && let Value::Str(value) = value
                    {
                        call_intent = crate::message::ToolCallIntent::new(value);
                    }
                }
                fields
            }
            Value::Unit => Vec::new(),
            other => {
                emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
                prepared.push(PreparedEntry::Failed {
                    index,
                    plane,
                    msg: build_error_result(
                        ctx,
                        &id,
                        &format!(
                            "tool `{name}` expected struct or unit input, got {}",
                            other.kind_name()
                        ),
                    ),
                });
                continue;
            }
        };
        let mut request_ctx = ctx.clone();
        request_ctx.model_mcp_scope = model_mcp_scope;
        let (name, tool, named) = if name == "mcp.call" {
            let args = ToolArgs {
                positional: Vec::new(),
                named,
            };
            match crate::tools::mcp::resolve_registered_call(&args, &request_ctx) {
                Ok((tool, args)) => (tool.name().to_string(), tool, args.named),
                Err(error) => {
                    emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
                    prepared.push(PreparedEntry::Failed {
                        index,
                        plane,
                        msg: build_error_result(ctx, &id, &error.to_string()),
                    });
                    continue;
                }
            }
        } else {
            (name, tool, named)
        };
        let plane = tool.invocation_plane();
        let raw_schema = tool.input_schema();
        emit_tool_node(
            ctx,
            &id,
            &name,
            &Value::Struct(named.clone()),
            call_intent.as_ref(),
        );
        if tool.requires_call_intent() && call_intent.is_none() {
            prepared.push(PreparedEntry::Failed {
                index,
                plane,
                msg: build_error_result(
                    ctx,
                    &id,
                    &format!(
                        "tool `{name}` was not executed because its required `_atman_intent` was missing or empty. Issue a new tool call with a concise `_atman_intent`; do not assume this call succeeded."
                    ),
                ),
            });
            continue;
        }
        let missing = missing_required_fields(&raw_schema, &named);
        if !missing.is_empty() {
            let content = format!(
                "tool `{name}` received empty/incomplete input. Missing required fields: {}. Retry with a complete argument object like {{{}}} — do NOT reuse an empty {{}} input.",
                missing.join(", "),
                missing
                    .iter()
                    .map(|field| format!("\"{field}\":\"...\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            prepared.push(PreparedEntry::Failed {
                index,
                plane,
                msg: build_error_result(ctx, &id, &content),
            });
            continue;
        }

        let call_args = ToolArgs {
            positional: Vec::new(),
            named,
        };
        let invocation_ctx = request_ctx
            .for_tool_invocation(tool.tier())
            .with_tool_use_id(id.clone())
            .with_call_intent(call_intent);
        let claims = match tool.scheduling_claims(&call_args, &invocation_ctx) {
            Ok(claims) => claims,
            Err(error) => {
                prepared.push(PreparedEntry::Failed {
                    index,
                    plane,
                    msg: build_error_result(
                        ctx,
                        &id,
                        &format!("tool `{name}` scheduling claims failed: {error}"),
                    ),
                });
                continue;
            }
        };
        prepared.push(PreparedEntry::Ready(Box::new(PreparedCall {
            index,
            id,
            name,
            tool,
            call_args,
            invocation_ctx,
            claims,
            plane,
        })));
    }
    Ok(prepared)
}

#[derive(Clone)]
struct DispatchOutcome {
    tool_result: Value,
    followups: Vec<Value>,
}

impl DispatchOutcome {
    fn tool_result(message: crate::message::Message) -> Self {
        Self {
            tool_result: Value::Host(AtmanPayload::Message(message)),
            followups: Vec::new(),
        }
    }
}

enum DispatchEpoch {
    Ordinary(Vec<PreparedEntry>),
    PermissionControl(PreparedEntry),
}

fn authorization_epochs(entries: Vec<PreparedEntry>) -> Vec<DispatchEpoch> {
    let mut epochs = Vec::new();
    let mut ordinary = Vec::new();
    for entry in entries {
        if entry.plane() == crate::tool::InvocationPlane::PermissionControl {
            if !ordinary.is_empty() {
                epochs.push(DispatchEpoch::Ordinary(std::mem::take(&mut ordinary)));
            }
            epochs.push(DispatchEpoch::PermissionControl(entry));
        } else {
            ordinary.push(entry);
        }
    }
    if !ordinary.is_empty() {
        epochs.push(DispatchEpoch::Ordinary(ordinary));
    }
    epochs
}

async fn dispatch_prepared(
    prepared: Vec<PreparedEntry>,
    ctx: &ToolCtx,
) -> Vec<Option<DispatchOutcome>> {
    let mut out_slots = vec![None; prepared.len()];
    let mut cancelled = false;
    for epoch in authorization_epochs(prepared) {
        if cancelled || ctx.cancel.is_cancelled() {
            cancel_epoch(epoch, ctx, &mut out_slots);
            cancelled = true;
            continue;
        }
        cancelled = match epoch {
            DispatchEpoch::Ordinary(calls) => run_ordinary_epoch(calls, ctx, &mut out_slots).await,
            DispatchEpoch::PermissionControl(call) => {
                run_permission_control_epoch(call, ctx, &mut out_slots).await
            }
        };
    }
    out_slots
}

fn cancel_epoch(epoch: DispatchEpoch, ctx: &ToolCtx, out_slots: &mut [Option<DispatchOutcome>]) {
    match epoch {
        DispatchEpoch::Ordinary(entries) => {
            for entry in entries {
                settle_unstarted_entry(entry, ctx, out_slots);
            }
        }
        DispatchEpoch::PermissionControl(entry) => settle_unstarted_entry(entry, ctx, out_slots),
    }
}

fn settle_unstarted_entry(
    entry: PreparedEntry,
    ctx: &ToolCtx,
    out_slots: &mut [Option<DispatchOutcome>],
) {
    match entry {
        PreparedEntry::Ready(call) => cancel_call(&call, ctx, out_slots),
        PreparedEntry::Failed { index, msg, .. } => {
            out_slots[index] = Some(DispatchOutcome::tool_result(emit_tool_result(ctx, &msg)));
        }
    }
}

fn cancel_call(call: &PreparedCall, ctx: &ToolCtx, out_slots: &mut [Option<DispatchOutcome>]) {
    if out_slots[call.index].is_none() {
        out_slots[call.index] = Some(finish_dispatch_outcome(
            ctx,
            &call.id,
            &call.name,
            call.tool.as_ref(),
            Err(RuntimeError::Cancelled(
                "flow cancelled before tool execution".into(),
            )),
        ));
    }
}

enum AuthorizationState {
    Pending,
    Approved(Box<ToolCtx>),
    Denied(String),
    Consumed,
}

type GateFuture = futures::future::BoxFuture<'static, (usize, Result<ToolCtx, String>)>;
type ExecutionFuture = futures::future::BoxFuture<'static, (usize, ToolResult)>;

fn gate_future(index: usize, call: &PreparedCall) -> GateFuture {
    let invocation_ctx = call.invocation_ctx.clone();
    let id = call.id.clone();
    let name = call.name.clone();
    let call_args = call.call_args.clone();
    let tool = std::sync::Arc::clone(&call.tool);
    Box::pin(async move {
        let outcome =
            authorize_tool_invocation(&invocation_ctx, &id, &name, &call_args, tool.as_ref()).await;
        (index, outcome)
    })
}

fn execution_future(index: usize, call: &PreparedCall, call_ctx: ToolCtx) -> ExecutionFuture {
    let call_args = call.call_args.clone();
    let tool = std::sync::Arc::clone(&call.tool);
    Box::pin(async move {
        let result = tool.call(call_args, &call_ctx).await;
        (index, result)
    })
}

fn advance_ready_nodes(
    calls: &[Box<PreparedCall>],
    authorizations: &mut [AuthorizationState],
    graph: &mut crate::tool_schedule::SchedulingGraph,
    running: &mut futures::stream::FuturesUnordered<ExecutionFuture>,
    ctx: &ToolCtx,
    out_slots: &mut [Option<DispatchOutcome>],
) -> bool {
    loop {
        let mut progressed = false;
        for index in graph.ready_wave() {
            if ctx.cancel.is_cancelled() {
                return true;
            }
            match std::mem::replace(&mut authorizations[index], AuthorizationState::Consumed) {
                AuthorizationState::Pending => {
                    authorizations[index] = AuthorizationState::Pending;
                }
                AuthorizationState::Approved(call_ctx) => {
                    graph
                        .start(index)
                        .expect("ready scheduling node must start");
                    running.push(execution_future(index, &calls[index], *call_ctx));
                    progressed = true;
                }
                AuthorizationState::Denied(reason) => {
                    let call = &calls[index];
                    let msg = build_error_result(
                        ctx,
                        &call.id,
                        &format!("tool `{}` denied: {reason}", call.name),
                    );
                    out_slots[call.index] =
                        Some(DispatchOutcome::tool_result(emit_tool_result(ctx, &msg)));
                    graph
                        .settle_ready(index)
                        .expect("ready denied scheduling node must settle");
                    progressed = true;
                }
                AuthorizationState::Consumed => {
                    debug_assert!(false, "consumed authorization cannot be graph-ready");
                }
            }
        }
        if !progressed {
            return false;
        }
    }
}

fn cancel_pending_ordinary_calls(
    calls: &[Box<PreparedCall>],
    graph: &crate::tool_schedule::SchedulingGraph,
    ctx: &ToolCtx,
    out_slots: &mut [Option<DispatchOutcome>],
) {
    for (index, call) in calls.iter().enumerate() {
        if graph.node_state(index) == Some(crate::tool_schedule::ScheduleNodeState::Pending) {
            cancel_call(call, ctx, out_slots);
        }
    }
}

async fn run_ordinary_epoch(
    entries: Vec<PreparedEntry>,
    ctx: &ToolCtx,
    out_slots: &mut [Option<DispatchOutcome>],
) -> bool {
    use futures::StreamExt;

    let mut calls = Vec::new();
    for entry in entries {
        match entry {
            PreparedEntry::Ready(call) => calls.push(call),
            PreparedEntry::Failed { index, msg, .. } => {
                out_slots[index] = Some(DispatchOutcome::tool_result(emit_tool_result(ctx, &msg)));
            }
        }
    }
    if calls.is_empty() {
        return ctx.cancel.is_cancelled();
    }

    let claims = calls
        .iter()
        .map(|call| call.claims.clone())
        .collect::<Vec<_>>();
    let mut graph = crate::tool_schedule::SchedulingGraph::from_claims(&claims);
    let mut authorizations = (0..calls.len())
        .map(|_| AuthorizationState::Pending)
        .collect::<Vec<_>>();
    let mut gates = futures::stream::FuturesUnordered::<GateFuture>::new();
    let mut running = futures::stream::FuturesUnordered::<ExecutionFuture>::new();

    if ctx.cancel.is_cancelled() {
        cancel_pending_ordinary_calls(&calls, &graph, ctx, out_slots);
        return true;
    }
    for (index, call) in calls.iter().enumerate() {
        gates.push(gate_future(index, call));
    }

    let mut cancelled = false;
    loop {
        if !cancelled
            && advance_ready_nodes(
                &calls,
                &mut authorizations,
                &mut graph,
                &mut running,
                ctx,
                out_slots,
            )
        {
            cancelled = true;
            gates = futures::stream::FuturesUnordered::new();
            cancel_pending_ordinary_calls(&calls, &graph, ctx, out_slots);
        }
        if cancelled {
            if running.is_empty() {
                break;
            }
        } else if graph.is_complete() {
            break;
        }

        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled(), if !cancelled => {
                cancelled = true;
                gates = futures::stream::FuturesUnordered::new();
                cancel_pending_ordinary_calls(&calls, &graph, ctx, out_slots);
            }
            gate = gates.next(), if !gates.is_empty() => {
                let (index, outcome) = gate.expect("non-empty gate set must yield");
                authorizations[index] = match outcome {
                    Ok(call_ctx) => AuthorizationState::Approved(Box::new(call_ctx)),
                    Err(reason) => AuthorizationState::Denied(reason),
                };
            }
            completed = running.next(), if !running.is_empty() => {
                let (index, result) = completed.expect("non-empty execution set must yield");
                let call = &calls[index];
                out_slots[call.index] = Some(finish_dispatch_outcome(
                    ctx,
                    &call.id,
                    &call.name,
                    call.tool.as_ref(),
                    result,
                ));
                graph
                    .complete(index)
                    .expect("running scheduling node must complete");
            }
            else => panic!("dispatch scheduler stalled with unsettled nodes"),
        }
    }
    cancelled
}

async fn run_permission_control_epoch(
    entry: PreparedEntry,
    ctx: &ToolCtx,
    out_slots: &mut [Option<DispatchOutcome>],
) -> bool {
    let call = match entry {
        PreparedEntry::Ready(call) => call,
        PreparedEntry::Failed { index, msg, .. } => {
            out_slots[index] = Some(DispatchOutcome::tool_result(emit_tool_result(ctx, &msg)));
            return ctx.cancel.is_cancelled();
        }
    };
    if ctx.cancel.is_cancelled() {
        cancel_call(&call, ctx, out_slots);
        return true;
    }
    let authorization = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            cancel_call(&call, ctx, out_slots);
            return true;
        }
        authorization = authorize_tool_invocation(
            &call.invocation_ctx,
            &call.id,
            &call.name,
            &call.call_args,
            call.tool.as_ref(),
        ) => authorization,
    };
    let call_ctx = match authorization {
        Ok(call_ctx) => call_ctx,
        Err(reason) => {
            let msg = build_error_result(
                ctx,
                &call.id,
                &format!("tool `{}` denied: {reason}", call.name),
            );
            out_slots[call.index] = Some(DispatchOutcome::tool_result(emit_tool_result(ctx, &msg)));
            return false;
        }
    };
    if ctx.cancel.is_cancelled() {
        cancel_call(&call, ctx, out_slots);
        return true;
    }

    let result = call.tool.call(call.call_args.clone(), &call_ctx).await;
    out_slots[call.index] = Some(finish_dispatch_outcome(
        ctx,
        &call.id,
        &call.name,
        call.tool.as_ref(),
        result,
    ));
    ctx.cancel.is_cancelled()
}

fn finish_dispatch_outcome(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    tool: &dyn Tool,
    result: ToolResult,
) -> DispatchOutcome {
    let followups = result
        .as_ref()
        .ok()
        .map(|value| {
            tool.model_followups(value, ctx)
                .into_iter()
                .map(|message| Value::Host(AtmanPayload::Message(message)))
                .collect()
        })
        .unwrap_or_default();
    DispatchOutcome {
        tool_result: finish_dispatch(ctx, id, name, result),
        followups,
    }
}

fn finish_dispatch(ctx: &ToolCtx, id: &str, name: &str, result: ToolResult) -> Value {
    let (content, is_error) = match &result {
        Ok(value) => (render_tool_result_text(value), false),
        Err(error) => (format!("{error}"), true),
    };
    if let Ok(value) = &result {
        emit_diff_preview_if_relevant(ctx, id, name, value);
    }
    let msg = crate::message::Message {
        role: crate::message::MessageRole::Tool,
        parts: vec![crate::message::MessagePart::ToolResult {
            tool_use_id: id.to_string(),
            content,
            is_error,
        }],
        turn_id: ctx
            .turn_id
            .clone()
            .unwrap_or_else(crate::event::TurnId::now),
        origin: crate::message::MessageOrigin::User,
    };
    Value::Host(AtmanPayload::Message(emit_tool_result(ctx, &msg)))
}

type DiffPreviewData = (String, Option<String>, Option<String>, Option<String>);

fn emit_diff_preview_if_relevant(ctx: &ToolCtx, tool_use_id: &str, tool_name: &str, value: &Value) {
    let Some(sink) = ctx.events.as_ref() else {
        return;
    };
    let data: Option<DiffPreviewData> = match tool_name {
        "fs.edit" => {
            let path = value_struct_string(value, "summary").and_then(|s| {
                s.strip_prefix("[fs.edit(")
                    .and_then(|s| s.split(':').next())
                    .map(|s| s.trim_end_matches(')').to_string())
            });
            let diff = value_struct_string(value, "diff");
            diff.map(|d| (path.unwrap_or_default(), None, None, Some(d)))
        }
        "fs.write" => {
            let path = value_struct_string(value, "path").unwrap_or_default();
            let diff = value_struct_string(value, "diff");
            diff.map(|d| (path, None, None, Some(d)))
        }
        "git.diff" => {
            let Some(diff) = value_struct_string(value, "diff") else {
                return;
            };
            Some(("git diff".into(), None, None, Some(diff)))
        }
        "git.show" => {
            let sha = value_struct_string(value, "sha").unwrap_or_default();
            let Some(diff) = value_struct_string(value, "diff") else {
                return;
            };
            Some((format!("git show {sha}"), None, None, Some(diff)))
        }
        "git.log" => {
            let Some(diff) = value_struct_string(value, "diff") else {
                return;
            };
            Some(("git log HEAD".into(), None, None, Some(diff)))
        }
        _ => None,
    };
    if let Some((title, old_content, new_content, unified_diff)) = data {
        sink.emit(crate::event::Event::DiffPreview {
            turn_id: ctx.turn_id.clone(),
            flow_run_id: ctx.flow_run_id.clone(),
            tool_use_id: Some(tool_use_id.to_owned()),
            title,
            old_content,
            new_content,
            unified_diff,
        });
    }
}

fn value_struct_string(value: &Value, field: &str) -> Option<String> {
    if let Value::Struct(fields) = value {
        fields
            .iter()
            .find(|(k, _)| k == field)
            .and_then(|(_, v)| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
    } else {
        None
    }
}

fn emit_tool_node(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    input: &Value,
    call_intent: Option<&crate::message::ToolCallIntent>,
) {
    let (Some(run_id), Some(parent_node)) = (&ctx.flow_run_id, &ctx.current_node_id) else {
        return;
    };
    let args_preview = format!("{:?}", input)
        .chars()
        .take(4000)
        .collect::<String>();
    if let Some(sink) = &ctx.events {
        sink.emit(crate::event::Event::ToolNode {
            run_id: run_id.clone(),
            parent_node_id: parent_node.clone(),
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            args_preview: args_preview.clone(),
            call_intent: call_intent.cloned(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolNode {
            run_id: run_id.0.to_string(),
            parent_node_id: parent_node.clone(),
            tool_use_id: id.to_string(),
            tool: name.to_string(),
            args_preview,
            call_intent: call_intent.cloned(),
        });
    }
}

fn build_error_result(ctx: &ToolCtx, tool_use_id: &str, content: &str) -> crate::message::Message {
    crate::message::Message {
        role: crate::message::MessageRole::Tool,
        parts: vec![crate::message::MessagePart::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error: true,
        }],
        turn_id: ctx
            .turn_id
            .clone()
            .unwrap_or_else(crate::event::TurnId::now),
        origin: crate::message::MessageOrigin::User,
    }
}

fn missing_required_fields(schema: &serde_json::Value, named: &[(String, Value)]) -> Vec<String> {
    let Some(required) = schema.get("required").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let have: std::collections::HashSet<&str> = named.iter().map(|(k, _)| k.as_str()).collect();
    required
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|k| !have.contains(k))
        .map(String::from)
        .collect()
}

fn emit_tool_result(ctx: &ToolCtx, msg: &crate::message::Message) -> crate::message::Message {
    let excerpt = crate::tools::tool_output::maybe_truncate_tool_message_with_budget(
        msg,
        ctx.output_store.as_deref(),
        ctx.tool_output_budget,
    );
    emit_tool_result_metrics(ctx, msg, &excerpt);
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolResultMsg {
            flow_run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
            message: excerpt.clone(),
        });
    } else if let Some(sink) = &ctx.events {
        sink.emit(crate::event::Event::ToolResultMsg {
            turn_id: excerpt.turn_id.clone(),
            flow_run_id: ctx.flow_run_id.clone(),
            message: excerpt.clone(),
        });
    }
    excerpt
}

fn emit_tool_result_metrics(
    ctx: &ToolCtx,
    raw: &crate::message::Message,
    excerpt: &crate::message::Message,
) {
    let Some(sink) = &ctx.events else {
        return;
    };
    for part in &raw.parts {
        let crate::message::MessagePart::ToolResult {
            tool_use_id,
            content: raw_content,
            ..
        } = part
        else {
            continue;
        };
        let excerpt_content = excerpt.parts.iter().find_map(|part| match part {
            crate::message::MessagePart::ToolResult {
                tool_use_id: excerpt_id,
                content,
                ..
            } if excerpt_id == tool_use_id => Some(content.as_str()),
            _ => None,
        });
        sink.emit(crate::event::Event::ToolResultMetrics {
            turn_id: raw.turn_id.clone(),
            flow_run_id: ctx.flow_run_id.clone(),
            tool_use_id: tool_use_id.clone(),
            raw_bytes: raw_content.len() as u64,
            excerpt_bytes: excerpt_content.map_or(0, |content| content.len() as u64),
            truncated: excerpt_content != Some(raw_content.as_str()),
        });
    }
}

fn render_tool_result_text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Host(AtmanPayload::Message(m)) => m.text_concat(),
        other => other
            .to_json()
            .map(|json| json.to_string())
            .unwrap_or_else(|error| format!("[JSON error: {error}]")),
    }
}

fn extract_list(args: &ToolArgs, name: &str, pos: usize) -> Result<Vec<Value>, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::List(items) => Ok(items.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "list".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorized_ctx(registry: std::sync::Arc<crate::tool::ToolRegistry>) -> ToolCtx {
        let trust = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Reckless,
            ..crate::trust::TrustConfig::default()
        };
        let flows = std::sync::Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
        let run_id = crate::event::FlowRunId::now();
        let identity = flows
            .register_root(
                "stdlib-test".into(),
                run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let mut ctx = ToolCtx::new()
            .with_registry(registry)
            .with_flow_registry(std::sync::Arc::clone(&flows))
            .with_permission_broker(crate::permission::PermissionBroker::shared(flows))
            .with_approval(std::sync::Arc::new(crate::session::ApprovalRegistry::new()))
            .with_trust(trust)
            .with_anchors(None, Some(run_id), None);
        ctx.flow_identity = Some(identity);
        ctx
    }

    struct PermitProbeTool;

    struct McpTargetProbe(&'static str);

    #[tokio::test]
    async fn pending_mcp_target_does_not_delay_other_batch_tools() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(PermitProbeTool));
        registry.register(std::sync::Arc::new(crate::tools::mcp::McpCall));
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.update_mcp_server(crate::mcp::McpServerStatus {
            name: "pending".into(),
            transport: crate::mcp::TransportKind::Stdio,
            state: crate::mcp::McpServerState::Connecting,
        });
        let ctx = authorized_ctx(std::sync::Arc::new(registry)).with_session_runtime(session);
        let uses = Value::from_json(serde_json::json!([
            {"id":"probe_id", "name":"permit.probe", "intent":"Inspect authorization", "input":{}},
            {"id":"pending_id", "name":"mcp.call", "intent":"Read remote data", "input":{"server":"pending", "tool":"read"}}
        ]));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            DispatchAll.call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            ),
        )
        .await
        .expect("batch preparation must not await MCP readiness")
        .unwrap();
        let Value::List(results) = result else {
            panic!("dispatch result list")
        };
        assert_eq!(results.len(), 2);
        for (index, result) in results.iter().enumerate() {
            let Value::Host(AtmanPayload::Message(message)) = result else {
                panic!("tool result message")
            };
            let [
                crate::message::MessagePart::ToolResult {
                    content, is_error, ..
                },
            ] = message.parts.as_slice()
            else {
                panic!("one tool result")
            };
            assert_eq!(*is_error, index == 1);
            if index == 0 {
                assert_eq!(content, "true");
            } else {
                assert!(content.contains("mcp.await"));
            }
        }
    }

    impl Tool for McpTargetProbe {
        fn name(&self) -> &str {
            self.0
        }

        fn tier(&self) -> Tier {
            Tier::Three
        }

        fn mcp_identity(&self) -> Option<(&str, &str)> {
            self.0
                .strip_prefix("mcp.")
                .and_then(|name| name.split_once('.'))
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object", "required":["value"]})
        }

        fn scheduling_claims(
            &self,
            args: &ToolArgs,
            _ctx: &ToolCtx,
        ) -> Result<crate::SchedulingClaims, RuntimeError> {
            assert!(matches!(args.named("value"), Some(Value::Int(7))));
            Ok(crate::SchedulingClaims::declared([
                crate::ResourceAccess::exclusive(crate::ResourceKey::Named(
                    "target-resource".into(),
                )),
            ]))
        }

        fn invocation_provenance(
            &self,
            _args: &ToolArgs,
            _ctx: &ToolCtx,
        ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
            Ok(crate::permission::ResourceProvenance::none().with_network())
        }

        fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                let permit = ctx
                    .invocation_authorization()
                    .expect("target authorization");
                assert!(permit.is_for_call(ctx.tool_use_id.as_deref().unwrap(), self.name()));
                assert!(permit.provenance().network);
                assert!(args.named("server").is_none());
                Ok(args.named("value").unwrap().clone())
            })
        }
    }

    #[tokio::test]
    async fn stale_mcp_name_recovers_through_schema_discovery_and_authorized_proxy() {
        use crate::message::{Message, MessageOrigin, MessagePart, MessageRole, ToolCallIntent};
        use crate::tool::{McpToolScope, ToolExposureRegistry};

        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(crate::tools::mcp::McpTools));
        registry.register(std::sync::Arc::new(crate::tools::mcp::McpCall));
        registry.register(std::sync::Arc::new(McpTargetProbe("mcp.docs.mutate")));
        let registry = std::sync::Arc::new(registry);
        let mut ctx = authorized_ctx(registry.clone());
        // Recovery and execution must use the producing request, not this default.
        ctx.model_mcp_scope = Some(McpToolScope::default());
        let exposures = ToolExposureRegistry::default();
        ctx.model_tool_exposures = Some(exposures.clone());
        let specs = ["mcp.tools", "mcp.call"]
            .map(|name| crate::tool::tool_spec(registry.get(name).unwrap().as_ref()));
        // A provider can return a historical wire name absent from this request.
        let stale_name = crate::tool_naming::from_wire("mcp_docs_mutate", &specs);
        let mut recovery_target = serde_json::Value::Null;
        for (id, name, input) in [
            ("stale", stale_name.as_str(), serde_json::json!({"value":7})),
            ("schema", "mcp.tools", serde_json::Value::Null),
            ("retry", "mcp.call", serde_json::Value::Null),
        ] {
            let input = match id {
                "schema" => recovery_target.clone(),
                "retry" => {
                    let mut input = recovery_target.clone();
                    input["input"] = serde_json::json!({"value":7});
                    input
                }
                _ => input,
            };
            let response = Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input,
                    intent: ToolCallIntent::new("Update the requested record"),
                }],
                turn_id: crate::event::TurnId::now(),
                origin: MessageOrigin::User,
            };
            exposures.register_response_with_mcp_scope(
                ctx.flow_run_id.as_ref(),
                &response,
                specs.iter().map(|spec| &spec.name),
                McpToolScope::Selectors(["mcp.docs.*".into()].into()),
            );
            let uses = ExtractToolUses
                .call(
                    ToolArgs {
                        positional: vec![Value::Host(AtmanPayload::Message(response))],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap();
            let Value::List(results) = DispatchAll
                .call(
                    ToolArgs {
                        positional: vec![uses],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap()
            else {
                panic!("dispatch results")
            };
            let Value::Host(AtmanPayload::Message(message)) = &results[0] else {
                panic!("tool message")
            };
            let MessagePart::ToolResult {
                content,
                is_error,
                tool_use_id,
                ..
            } = &message.parts[0]
            else {
                panic!("tool result")
            };
            assert_eq!(tool_use_id, id);
            match id {
                "stale" => {
                    assert!(*is_error);
                    assert!(content.starts_with("tool_not_exposed:"));
                    assert!(!content.contains("ambiguous"));
                    assert!(ctx.permission_broker.as_ref().unwrap().list().is_empty());
                    recovery_target =
                        crate::eval::llm_parse::extract_json_from_text(content).unwrap();
                    assert_eq!(
                        recovery_target,
                        serde_json::json!({"server":"docs", "tool":"mutate"})
                    );
                }
                "schema" => {
                    assert!(!is_error, "{content}");
                    let schema: serde_json::Value = serde_json::from_str(content).unwrap();
                    assert_eq!(schema["tools"][0]["input_schema"]["required"][0], "value");
                }
                _ => {
                    assert!(!is_error, "{content}");
                    assert_eq!(content, "7");
                }
            }
        }
    }

    async fn assert_colliding_mcp_scopes_fail_closed(scopes: [crate::tool::McpToolScope; 2]) {
        use crate::message::{Message, MessageOrigin, MessagePart, MessageRole, ToolCallIntent};

        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(crate::tools::mcp::McpCall));
        registry.register(std::sync::Arc::new(McpTargetProbe("mcp.secret.mutate")));
        let mut ctx = authorized_ctx(std::sync::Arc::new(registry));
        let exposures = crate::tool::ToolExposureRegistry::default();
        ctx.model_tool_exposures = Some(exposures.clone());
        let responses = ["first", "second"].map(|label| Message {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::ToolUse {
                id: "reused-id".into(),
                name: "mcp.call".into(),
                input: serde_json::json!({"server":"secret", "tool":"mutate", "input":{"value":7}}),
                intent: ToolCallIntent::new(label),
            }],
            turn_id: crate::event::TurnId::now(),
            origin: MessageOrigin::User,
        });
        for (response, scope) in responses.iter().zip(scopes) {
            exposures.register_response_with_mcp_scope(
                ctx.flow_run_id.as_ref(),
                response,
                ["mcp.call"],
                scope,
            );
        }
        for response in responses.iter().rev() {
            let uses = ExtractToolUses
                .call(
                    ToolArgs {
                        positional: vec![Value::Host(AtmanPayload::Message(response.clone()))],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap();
            let Value::List(results) = DispatchAll
                .call(
                    ToolArgs {
                        positional: vec![uses],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap()
            else {
                panic!("dispatch results")
            };
            assert!(matches!(results.as_slice(),
                [Value::Host(AtmanPayload::Message(message))]
                    if matches!(message.parts.as_slice(),
                        [MessagePart::ToolResult { is_error: true, content, .. }]
                            if content.contains("ambiguous")
                    )
            ));
        }
        assert!(ctx.permission_broker.as_ref().unwrap().list().is_empty());

        // Consuming every colliding call removes the ambiguity. Sequential
        // responses may then reuse the provider's ID without losing their scope.
        for _ in 0..2 {
            exposures.register_response_with_mcp_scope(
                ctx.flow_run_id.as_ref(),
                &responses[0],
                ["mcp.call"],
                crate::tool::McpToolScope::All,
            );
            assert_eq!(
                exposures.claim_with_mcp_scope(ctx.flow_run_id.as_ref(), "reused-id", "mcp.call"),
                Ok(crate::tool::McpToolScope::All)
            );
        }
    }

    #[tokio::test]
    async fn mcp_proxy_rejects_restricted_response_before_colliding_broad_response() {
        assert_colliding_mcp_scopes_fail_closed([
            crate::tool::McpToolScope::All,
            crate::tool::McpToolScope::Selectors(["mcp.docs.*".into()].into()),
        ])
        .await;
    }

    #[tokio::test]
    async fn mcp_proxy_rejects_broad_response_before_colliding_restricted_response() {
        assert_colliding_mcp_scopes_fail_closed([
            crate::tool::McpToolScope::Selectors(["mcp.docs.*".into()].into()),
            crate::tool::McpToolScope::All,
        ])
        .await;
    }

    #[tokio::test]
    async fn mcp_proxy_dispatch_preserves_target_contract_and_request_scope() {
        use crate::message::{Message, MessageOrigin, MessagePart, MessageRole, ToolCallIntent};

        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(crate::tools::mcp::McpCall));
        registry.register(std::sync::Arc::new(McpTargetProbe("mcp.docs.mutate")));
        registry.register(std::sync::Arc::new(McpTargetProbe("mcp.secret.mutate")));
        let sink = crate::event::EventSink::new();
        let mut ctx = authorized_ctx(std::sync::Arc::new(registry))
            .with_events(sink.clone())
            .with_current_node(Some("dispatch".into()));
        let response = Message {
            role: MessageRole::Assistant,
            parts: [
                ("direct", "mcp.docs.mutate", serde_json::json!({"value":7})),
                (
                    "proxy",
                    "mcp.call",
                    serde_json::json!({"server":"docs", "tool":"mutate", "input":{"value":7}}),
                ),
                (
                    "missing",
                    "mcp.call",
                    serde_json::json!({"server":"docs", "tool":"mutate", "input":{}}),
                ),
                (
                    "forbidden",
                    "mcp.call",
                    serde_json::json!({"server":"secret", "tool":"mutate", "input":{"value":7}}),
                ),
            ]
            .into_iter()
            .map(|(id, name, input)| MessagePart::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
                intent: ToolCallIntent::new("Update the requested record"),
            })
            .collect(),
            turn_id: crate::event::TurnId::now(),
            origin: MessageOrigin::User,
        };
        let exposures = crate::tool::ToolExposureRegistry::default();
        exposures.register_response_with_mcp_scope(
            ctx.flow_run_id.as_ref(),
            &response,
            ["mcp.call", "mcp.docs.mutate"],
            crate::tool::McpToolScope::Selectors(["mcp.docs.*".into()].into()),
        );
        ctx.model_tool_exposures = Some(exposures);
        let Value::List(uses) = ExtractToolUses
            .call(
                ToolArgs {
                    positional: vec![Value::Host(AtmanPayload::Message(response))],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("tool uses");
        };
        let prepared = prepare_dispatch(&uses, ctx.registry.as_ref().unwrap(), &ctx).unwrap();
        let [
            PreparedEntry::Ready(direct),
            PreparedEntry::Ready(proxy),
            PreparedEntry::Failed { .. },
            PreparedEntry::Failed { .. },
        ] = prepared.as_slice()
        else {
            panic!("two valid calls, one incomplete call, one forbidden call");
        };
        assert_eq!(direct.name, "mcp.docs.mutate");
        assert_eq!(proxy.name, direct.name);
        assert_eq!(proxy.tool.tier(), Tier::Three);
        assert_eq!(proxy.claims, direct.claims);
        assert_eq!(proxy.id, "proxy");
        assert!(sink.snapshot().iter().any(|event| matches!(event,
            crate::event::Event::ToolNode { tool_use_id, tool_name, .. }
                if tool_use_id == "proxy" && tool_name == "mcp.docs.mutate"
        )));
        let results = dispatch_prepared(prepared, &ctx).await;
        for (index, id) in ["direct", "proxy", "missing", "forbidden"]
            .iter()
            .enumerate()
        {
            let Value::Host(AtmanPayload::Message(message)) =
                &results[index].as_ref().unwrap().tool_result
            else {
                panic!("result message");
            };
            let [
                MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                },
            ] = message.parts.as_slice()
            else {
                panic!("tool result");
            };
            assert_eq!(tool_use_id, id);
            assert_eq!(*is_error, index >= 2);
            match index {
                0 | 1 => assert_eq!(content, "7"),
                2 => assert!(content.contains("Missing required fields: value")),
                _ => assert!(content.contains("was not allowed")),
            }
        }
        let requests = ctx.permission_broker.as_ref().unwrap().list();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request.intent.tool_name == "mcp.docs.mutate"
                && request.intent.tier == Tier::Three
                && request.intent.provenance.network
        }));
        assert_eq!(
            requests[0].intent.args_digest,
            requests[1].intent.args_digest
        );
    }

    impl Tool for PermitProbeTool {
        fn name(&self) -> &str {
            "permit.probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                let authorized = ctx
                    .invocation_authorization()
                    .is_some_and(|permit| permit.is_for_call("probe_id", "permit.probe"));
                Ok(Value::Bool(authorized))
            })
        }
    }

    struct ControlProbeTool;

    impl Tool for ControlProbeTool {
        fn name(&self) -> &str {
            "permission.probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn invocation_plane(&self) -> crate::tool::InvocationPlane {
            crate::tool::InvocationPlane::PermissionControl
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { Ok(Value::Unit) })
        }
    }

    struct UnexposedProbeTool;

    impl Tool for UnexposedProbeTool {
        fn name(&self) -> &str {
            "hidden.probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { panic!("unexposed tool must not execute") })
        }
    }

    struct FollowupProbeTool(&'static str);

    impl Tool for FollowupProbeTool {
        fn name(&self) -> &str {
            self.0
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn model_followups(&self, result: &Value, _ctx: &ToolCtx) -> Vec<crate::message::Message> {
            let Value::Str(label) = result else {
                return Vec::new();
            };
            let mut message = crate::message::Message::user_text(
                crate::event::TurnId::now(),
                format!("followup:{label}"),
            );
            message.origin = crate::message::MessageOrigin::Internal;
            vec![message]
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move { Ok(Value::Str(self.0.to_string())) })
        }
    }

    #[tokio::test]
    async fn dispatch_all_places_followups_after_the_complete_tool_result_batch() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(FollowupProbeTool("probe.first")));
        registry.register(std::sync::Arc::new(FollowupProbeTool("probe.second")));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let uses = Value::List(
            ["probe.first", "probe.second"]
                .into_iter()
                .enumerate()
                .map(|(index, name)| {
                    Value::Struct(vec![
                        ("id".into(), Value::Str(format!("call-{index}"))),
                        ("name".into(), Value::Str(name.into())),
                        ("intent".into(), Value::Str(format!("Run {name}"))),
                        ("input".into(), Value::Struct(Vec::new())),
                    ])
                })
                .collect(),
        );

        let Value::List(messages) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("expected messages")
        };

        assert_eq!(messages.len(), 4);
        assert!(messages[..2].iter().all(
            |value| matches!(value, Value::Host(AtmanPayload::Message(message)) if message.role == crate::message::MessageRole::Tool)
        ));
        assert_eq!(
            messages[2..]
                .iter()
                .map(|value| match value {
                    Value::Host(AtmanPayload::Message(message)) => message.text_concat(),
                    _ => panic!("expected message"),
                })
                .collect::<Vec<_>>(),
            ["followup:probe.first", "followup:probe.second"]
        );
    }

    #[tokio::test]
    async fn dispatch_all_rejects_calls_outside_their_request_exposure() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(UnexposedProbeTool));
        let mut ctx = authorized_ctx(std::sync::Arc::new(registry));
        let response = crate::message::Message {
            role: crate::message::MessageRole::Assistant,
            parts: vec![crate::message::MessagePart::ToolUse {
                id: "hidden-call".into(),
                name: "hidden.probe".into(),
                input: serde_json::json!({}),
                intent: None,
            }],
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        };
        let exposures = crate::tool::ToolExposureRegistry::default();
        exposures.register_response(ctx.flow_run_id.as_ref(), &response, ["allowed.probe"]);
        ctx.model_tool_exposures = Some(exposures);
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("hidden-call".into())),
            ("name".into(), Value::Str("hidden.probe".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list")
        };
        assert!(matches!(
            &results[0],
            Value::Host(AtmanPayload::Message(crate::message::Message { parts, .. }))
                if matches!(
                    &parts[..],
                    [crate::message::MessagePart::ToolResult { content, is_error: true, .. }]
                        if content.contains("was not exposed")
                )
        ));
    }

    #[tokio::test]
    async fn dispatch_all_runs_permission_control_without_recursive_requests() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(ControlProbeTool));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let before = ctx.permission_broker.as_ref().unwrap().list().len();
        let uses = Value::List(vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("control-1".into())),
                ("name".into(), Value::Str("permission.probe".into())),
                (
                    "intent".into(),
                    Value::Str("Inspect permission control".into()),
                ),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
            Value::Struct(vec![
                ("id".into(), Value::Str("control-2".into())),
                ("name".into(), Value::Str("permission.probe".into())),
                (
                    "intent".into(),
                    Value::Str("Inspect permission control".into()),
                ),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
        ]);
        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list")
        };

        assert_eq!(results.len(), 2);
        assert_eq!(ctx.permission_broker.as_ref().unwrap().list().len(), before);
    }

    #[tokio::test]
    async fn auto_parallel_call_receives_its_own_authorization() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(PermitProbeTool));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("probe_id".into())),
            ("name".into(), Value::Str("permit.probe".into())),
            ("intent".into(), Value::Str("Inspect authorization".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Host(AtmanPayload::Message(message)) = &results[0] else {
            panic!("tool result message");
        };
        assert!(message.parts.iter().any(|part| matches!(
            part,
            crate::message::MessagePart::ToolResult { content, is_error: false, .. }
                if content == "true"
        )));
    }

    #[tokio::test]
    async fn dispatch_all_strips_wire_intent_and_isolates_parallel_contexts() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(PermitProbeTool));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let uses = vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("first".into())),
                ("name".into(), Value::Str("permit.probe".into())),
                (
                    "input".into(),
                    Value::Struct(vec![(
                        crate::message::TOOL_CALL_INTENT_FIELD.into(),
                        Value::Str("Inspect first target".into()),
                    )]),
                ),
            ]),
            Value::Struct(vec![
                ("id".into(), Value::Str("second".into())),
                ("name".into(), Value::Str("permit.probe".into())),
                (
                    "input".into(),
                    Value::Struct(vec![(
                        crate::message::TOOL_CALL_INTENT_FIELD.into(),
                        Value::Str("Inspect second target".into()),
                    )]),
                ),
            ]),
        ];
        let prepared = prepare_dispatch(&uses, ctx.registry.as_ref().unwrap(), &ctx).unwrap();
        let [PreparedEntry::Ready(first), PreparedEntry::Ready(second)] = prepared.as_slice()
        else {
            panic!("two ready calls")
        };

        assert!([first, second].iter().all(|call| {
            call.call_args
                .named(crate::message::TOOL_CALL_INTENT_FIELD)
                .is_none()
        }));
        assert_eq!(
            first
                .invocation_ctx
                .call_intent
                .as_ref()
                .map(|intent| intent.as_str()),
            Some("Inspect first target")
        );
        assert_eq!(
            second
                .invocation_ctx
                .call_intent
                .as_ref()
                .map(|intent| intent.as_str()),
            Some("Inspect second target")
        );
    }

    struct ControlledTool {
        name: &'static str,
        release: std::sync::Arc<tokio::sync::Semaphore>,
    }

    impl Tool for ControlledTool {
        fn name(&self) -> &str {
            self.name
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn scheduling_claims(
            &self,
            _args: &ToolArgs,
            _ctx: &ToolCtx,
        ) -> Result<crate::SchedulingClaims, RuntimeError> {
            Ok(crate::SchedulingClaims::declared([
                crate::ResourceAccess::read(crate::ResourceKey::Named(self.name.to_owned())),
            ]))
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                let _permit = self.release.acquire().await.unwrap();
                Ok(Value::Str(self.name.to_string()))
            })
        }
    }

    struct SchedulingProbeTool {
        name: &'static str,
        tier: Tier,
        plane: crate::tool::InvocationPlane,
        claims: crate::SchedulingClaims,
        started: tokio::sync::mpsc::UnboundedSender<&'static str>,
        release: std::sync::Arc<tokio::sync::Semaphore>,
    }

    impl Tool for SchedulingProbeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn tier(&self) -> Tier {
            self.tier
        }

        fn invocation_plane(&self) -> crate::tool::InvocationPlane {
            self.plane
        }

        fn scheduling_claims(
            &self,
            _args: &ToolArgs,
            _ctx: &ToolCtx,
        ) -> Result<crate::SchedulingClaims, RuntimeError> {
            Ok(self.claims.clone())
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                let _ = self.started.send(self.name);
                let _permit = self.release.acquire().await.unwrap();
                Ok(Value::Str(self.name.to_owned()))
            })
        }
    }

    struct ClaimsProbeTool {
        name: &'static str,
        plane: crate::tool::InvocationPlane,
        claims_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        execution_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_claims: bool,
    }

    impl Tool for ClaimsProbeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn invocation_plane(&self) -> crate::tool::InvocationPlane {
            self.plane
        }

        fn scheduling_claims(
            &self,
            _args: &ToolArgs,
            _ctx: &ToolCtx,
        ) -> Result<crate::SchedulingClaims, RuntimeError> {
            self.claims_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_claims {
                Err(RuntimeError::ToolFailed("claims probe failed".into()))
            } else {
                Ok(crate::SchedulingClaims::declared([]))
            }
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            self.execution_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok(Value::Str(self.name.to_owned())) })
        }
    }

    fn named_read(name: &str) -> crate::SchedulingClaims {
        crate::SchedulingClaims::declared([crate::ResourceAccess::read(crate::ResourceKey::Named(
            name.to_owned(),
        ))])
    }

    fn named_write(name: &str) -> crate::SchedulingClaims {
        crate::SchedulingClaims::declared([crate::ResourceAccess::write(
            crate::ResourceKey::Named(name.to_owned()),
        )])
    }

    fn probe_use(id: &str, name: &str) -> Value {
        Value::Struct(vec![
            ("id".into(), Value::Str(id.to_owned())),
            ("name".into(), Value::Str(name.to_owned())),
            ("intent".into(), Value::Str(format!("Run {name}"))),
            ("input".into(), Value::Struct(Vec::new())),
        ])
    }

    fn spawn_dispatch(ctx: ToolCtx, uses: Vec<Value>) -> tokio::task::JoinHandle<Vec<Value>> {
        tokio::spawn(async move {
            let Value::List(results) = DispatchAll
                .call(
                    ToolArgs {
                        positional: vec![Value::List(uses)],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap()
            else {
                panic!("dispatch result list")
            };
            results
        })
    }

    async fn recv_started(
        started: &mut tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    ) -> &'static str {
        tokio::time::timeout(std::time::Duration::from_secs(1), started.recv())
            .await
            .expect("tool start")
            .expect("start sender")
    }

    async fn assert_no_start(started: &mut tokio::sync::mpsc::UnboundedReceiver<&'static str>) {
        match tokio::time::timeout(std::time::Duration::from_millis(50), started.recv()).await {
            Err(_) | Ok(None) => {}
            Ok(Some(name)) => panic!("unexpected tool start: {name}"),
        }
    }

    fn approval_ctx(
        registry: std::sync::Arc<crate::tool::ToolRegistry>,
    ) -> (ToolCtx, crate::permission::PermissionClientGuard) {
        let trust = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Steady,
            ..crate::trust::TrustConfig::default()
        };
        let flows = std::sync::Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
        let broker = crate::permission::PermissionBroker::shared(std::sync::Arc::clone(&flows));
        let client = broker.register_client();
        let run_id = crate::event::FlowRunId::now();
        let identity = flows
            .register_root(
                "dispatch-test".into(),
                run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let mut ctx = ToolCtx::new()
            .with_registry(registry)
            .with_flow_registry(flows)
            .with_permission_broker(broker)
            .with_approval(std::sync::Arc::new(crate::session::ApprovalRegistry::new()))
            .with_trust(trust)
            .with_anchors(None, Some(run_id), None);
        ctx.flow_identity = Some(identity);
        (ctx, client)
    }

    async fn wait_for_permission(
        broker: &crate::permission::PermissionBroker,
        tool_name: &str,
    ) -> crate::permission::PermissionRequestId {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(request) = broker.list().into_iter().find(|request| {
                    request.intent.tool_name == tool_name
                        && matches!(
                            request.state,
                            crate::permission::PermissionRequestState::Pending { .. }
                        )
                }) {
                    break request.request_id.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permission request")
    }

    fn resolve_permission(
        ctx: &ToolCtx,
        request_id: &crate::permission::PermissionRequestId,
        action: crate::permission::PermissionAction,
    ) {
        let broker = ctx.permission_broker.as_ref().unwrap();
        let identity = ctx.flow_identity.as_ref().unwrap();
        broker
            .resolve(
                request_id,
                &crate::permission::DecisionAuthority::User(
                    broker.user_authority(identity.session_id.clone(), Some("test-user".into())),
                ),
                action,
                (action == crate::permission::PermissionAction::Approve)
                    .then_some(crate::permission::GrantScope::CurrentCall),
                (action == crate::permission::PermissionAction::Deny)
                    .then(|| "denied by test".to_owned()),
            )
            .unwrap();
    }

    fn tool_result(value: &Value) -> (&str, &str, bool) {
        let Value::Host(AtmanPayload::Message(message)) = value else {
            panic!("tool result message")
        };
        let crate::message::MessagePart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = &message.parts[0]
        else {
            panic!("tool result part")
        };
        (tool_use_id, content, *is_error)
    }

    #[tokio::test]
    async fn independent_claims_start_in_the_same_ready_wave() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let second_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "independent.first",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_read("resource-a"),
            started: started_tx.clone(),
            release: first_release.clone(),
        }));
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "independent.second",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_read("resource-b"),
            started: started_tx,
            release: second_release.clone(),
        }));
        let task = spawn_dispatch(
            authorized_ctx(std::sync::Arc::new(registry)),
            vec![
                probe_use("first", "independent.first"),
                probe_use("second", "independent.second"),
            ],
        );

        assert_eq!(recv_started(&mut started_rx).await, "independent.first");
        assert_eq!(recv_started(&mut started_rx).await, "independent.second");
        first_release.add_permits(1);
        second_release.add_permits(1);

        let results = task.await.unwrap();
        assert_eq!(tool_result(&results[0]).0, "first");
        assert_eq!(tool_result(&results[1]).0, "second");
    }

    #[tokio::test]
    async fn conflicting_claims_preserve_source_order() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let second_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "conflict.write",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_write("shared"),
            started: started_tx.clone(),
            release: first_release.clone(),
        }));
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "conflict.read",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_read("shared"),
            started: started_tx,
            release: second_release.clone(),
        }));
        let task = spawn_dispatch(
            authorized_ctx(std::sync::Arc::new(registry)),
            vec![
                probe_use("write", "conflict.write"),
                probe_use("read", "conflict.read"),
            ],
        );

        assert_eq!(recv_started(&mut started_rx).await, "conflict.write");
        assert_no_start(&mut started_rx).await;
        first_release.add_permits(1);
        assert_eq!(recv_started(&mut started_rx).await, "conflict.read");
        second_release.add_permits(1);

        let results = task.await.unwrap();
        assert_eq!(tool_result(&results[0]).0, "write");
        assert_eq!(tool_result(&results[1]).0, "read");
    }

    #[tokio::test]
    async fn opaque_claim_is_a_bidirectional_execution_barrier() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let opaque_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let last_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        for (name, claims, release) in [
            ("opaque.first", named_read("a"), first_release.clone()),
            (
                "opaque.barrier",
                crate::SchedulingClaims::opaque(),
                opaque_release.clone(),
            ),
            ("opaque.last", named_read("b"), last_release.clone()),
        ] {
            registry.register(std::sync::Arc::new(SchedulingProbeTool {
                name,
                tier: Tier::Zero,
                plane: crate::tool::InvocationPlane::Ordinary,
                claims,
                started: started_tx.clone(),
                release,
            }));
        }
        let task = spawn_dispatch(
            authorized_ctx(std::sync::Arc::new(registry)),
            vec![
                probe_use("first", "opaque.first"),
                probe_use("barrier", "opaque.barrier"),
                probe_use("last", "opaque.last"),
            ],
        );

        assert_eq!(recv_started(&mut started_rx).await, "opaque.first");
        assert_no_start(&mut started_rx).await;
        first_release.add_permits(1);
        assert_eq!(recv_started(&mut started_rx).await, "opaque.barrier");
        assert_no_start(&mut started_rx).await;
        opaque_release.add_permits(1);
        assert_eq!(recv_started(&mut started_rx).await, "opaque.last");
        last_release.add_permits(1);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn pending_conflicting_approval_blocks_successor_and_denial_releases_it() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let denied_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let successor_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "approval.write",
            tier: Tier::Two,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_write("approval-shared"),
            started: started_tx.clone(),
            release: denied_release,
        }));
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "approval.read",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_read("approval-shared"),
            started: started_tx,
            release: successor_release.clone(),
        }));
        let (ctx, _client) = approval_ctx(std::sync::Arc::new(registry));
        let broker = std::sync::Arc::clone(ctx.permission_broker.as_ref().unwrap());
        let task = spawn_dispatch(
            ctx.clone(),
            vec![
                probe_use("write", "approval.write"),
                probe_use("read", "approval.read"),
            ],
        );

        let request_id = wait_for_permission(&broker, "approval.write").await;
        assert_no_start(&mut started_rx).await;
        resolve_permission(&ctx, &request_id, crate::permission::PermissionAction::Deny);
        assert_eq!(recv_started(&mut started_rx).await, "approval.read");
        successor_release.add_permits(1);

        let results = task.await.unwrap();
        assert!(tool_result(&results[0]).2);
        assert!(!tool_result(&results[1]).2);
    }

    #[tokio::test]
    async fn permission_control_epoch_blocks_later_gating_until_execution_finishes() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let control_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let after_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        for (name, tier, plane, release) in [
            (
                "epoch.first",
                Tier::Zero,
                crate::tool::InvocationPlane::Ordinary,
                first_release.clone(),
            ),
            (
                "epoch.control",
                Tier::Zero,
                crate::tool::InvocationPlane::PermissionControl,
                control_release.clone(),
            ),
            (
                "epoch.after",
                Tier::Two,
                crate::tool::InvocationPlane::Ordinary,
                after_release.clone(),
            ),
        ] {
            registry.register(std::sync::Arc::new(SchedulingProbeTool {
                name,
                tier,
                plane,
                claims: named_read(name),
                started: started_tx.clone(),
                release,
            }));
        }
        let (ctx, _client) = approval_ctx(std::sync::Arc::new(registry));
        let broker = std::sync::Arc::clone(ctx.permission_broker.as_ref().unwrap());
        let task = spawn_dispatch(
            ctx.clone(),
            vec![
                probe_use("first", "epoch.first"),
                probe_use("control", "epoch.control"),
                probe_use("after", "epoch.after"),
            ],
        );

        assert_eq!(recv_started(&mut started_rx).await, "epoch.first");
        assert!(
            broker
                .list()
                .iter()
                .all(|request| request.intent.tool_name != "epoch.after")
        );
        assert_no_start(&mut started_rx).await;
        first_release.add_permits(1);
        assert_eq!(recv_started(&mut started_rx).await, "epoch.control");
        assert!(
            broker
                .list()
                .iter()
                .all(|request| request.intent.tool_name != "epoch.after")
        );
        assert_no_start(&mut started_rx).await;
        control_release.add_permits(1);

        let request_id = wait_for_permission(&broker, "epoch.after").await;
        resolve_permission(
            &ctx,
            &request_id,
            crate::permission::PermissionAction::Approve,
        );
        assert_eq!(recv_started(&mut started_rx).await, "epoch.after");
        after_release.add_permits(1);
        let results = task.await.unwrap();
        assert!(results.iter().all(|result| !tool_result(result).2));
    }

    #[tokio::test]
    async fn scheduling_claims_are_evaluated_once_and_errors_keep_their_result_slot() {
        let success_claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let success_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failure_claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failure_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(ClaimsProbeTool {
            name: "claims.success",
            plane: crate::tool::InvocationPlane::Ordinary,
            claims_calls: success_claims.clone(),
            execution_calls: success_calls.clone(),
            fail_claims: false,
        }));
        registry.register(std::sync::Arc::new(ClaimsProbeTool {
            name: "claims.failure",
            plane: crate::tool::InvocationPlane::Ordinary,
            claims_calls: failure_claims.clone(),
            execution_calls: failure_calls.clone(),
            fail_claims: true,
        }));

        let results = spawn_dispatch(
            authorized_ctx(std::sync::Arc::new(registry)),
            vec![
                probe_use("success", "claims.success"),
                probe_use("failure", "claims.failure"),
            ],
        )
        .await
        .unwrap();

        assert_eq!(success_claims.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(success_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(failure_claims.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(failure_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(tool_result(&results[0]).0, "success");
        assert!(!tool_result(&results[0]).2);
        assert_eq!(tool_result(&results[1]).0, "failure");
        assert!(tool_result(&results[1]).2);
        assert!(tool_result(&results[1]).1.contains("claims probe failed"));
    }

    #[tokio::test]
    async fn failed_permission_control_claim_still_separates_ordinary_epochs() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let after_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let claims_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let execution_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "failed-control.first",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_read("first"),
            started: started_tx.clone(),
            release: first_release.clone(),
        }));
        registry.register(std::sync::Arc::new(ClaimsProbeTool {
            name: "failed-control.control",
            plane: crate::tool::InvocationPlane::PermissionControl,
            claims_calls: claims_calls.clone(),
            execution_calls: execution_calls.clone(),
            fail_claims: true,
        }));
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "failed-control.after",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_read("after"),
            started: started_tx,
            release: after_release.clone(),
        }));
        let task = spawn_dispatch(
            authorized_ctx(std::sync::Arc::new(registry)),
            vec![
                probe_use("first", "failed-control.first"),
                probe_use("control", "failed-control.control"),
                probe_use("after", "failed-control.after"),
            ],
        );

        assert_eq!(recv_started(&mut started_rx).await, "failed-control.first");
        assert_no_start(&mut started_rx).await;
        first_release.add_permits(1);
        assert_eq!(recv_started(&mut started_rx).await, "failed-control.after");
        after_release.add_permits(1);

        let results = task.await.unwrap();
        assert_eq!(claims_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(execution_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(tool_result(&results[1]).2);
    }

    #[tokio::test]
    async fn cancellation_does_not_start_newly_ready_nodes() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let second_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "cancel.first",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_write("cancel-shared"),
            started: started_tx.clone(),
            release: first_release.clone(),
        }));
        registry.register(std::sync::Arc::new(SchedulingProbeTool {
            name: "cancel.second",
            tier: Tier::Zero,
            plane: crate::tool::InvocationPlane::Ordinary,
            claims: named_read("cancel-shared"),
            started: started_tx,
            release: second_release,
        }));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let cancel = ctx.cancel.clone();
        let task = spawn_dispatch(
            ctx,
            vec![
                probe_use("first", "cancel.first"),
                probe_use("second", "cancel.second"),
            ],
        );

        assert_eq!(recv_started(&mut started_rx).await, "cancel.first");
        cancel.cancel();
        first_release.add_permits(1);
        assert_no_start(&mut started_rx).await;

        let results = task.await.unwrap();
        assert!(!tool_result(&results[0]).2);
        assert!(tool_result(&results[1]).2);
        assert!(tool_result(&results[1]).1.contains("cancelled"));
    }

    #[tokio::test]
    async fn dispatch_all_emits_each_scoped_result_as_its_tool_finishes() {
        let fast_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let slow_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(ControlledTool {
            name: "fast",
            release: fast_release.clone(),
        }));
        registry.register(std::sync::Arc::new(ControlledTool {
            name: "slow",
            release: slow_release.clone(),
        }));
        let run_id = crate::event::FlowRunId::now();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(32);
        let ctx = authorized_ctx(std::sync::Arc::new(registry))
            .with_anchors(None, Some(run_id.clone()), None)
            .with_current_node(Some("dispatch_all".into()))
            .with_stream_tx(stream_tx);
        let uses = Value::List(vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("slow_id".into())),
                ("name".into(), Value::Str("slow".into())),
                ("intent".into(), Value::Str("Run slow probe".into())),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
            Value::Struct(vec![
                ("id".into(), Value::Str("fast_id".into())),
                ("name".into(), Value::Str("fast".into())),
                ("intent".into(), Value::Str("Run fast probe".into())),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
        ]);
        let task = tokio::spawn(async move {
            DispatchAll
                .call(
                    ToolArgs {
                        positional: vec![uses],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap()
        });

        fast_release.add_permits(1);
        let fast_result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let crate::stream::StreamFrame::ToolResultMsg {
                    flow_run_id,
                    message,
                } = stream_rx.recv().await.unwrap()
                    && message.parts.iter().any(|part| {
                        matches!(
                            part,
                            crate::message::MessagePart::ToolResult { tool_use_id, .. }
                                if tool_use_id == "fast_id"
                        )
                    })
                {
                    break flow_run_id;
                }
            }
        })
        .await
        .expect("fast result before slow release");
        assert_eq!(fast_result.as_deref(), Some(run_id.0.to_string().as_str()));
        assert!(!task.is_finished());

        slow_release.add_permits(1);
        let Value::List(results) = task.await.unwrap() else {
            panic!("dispatch result list");
        };
        let ids: Vec<&str> = results
            .iter()
            .map(|value| match value {
                Value::Host(AtmanPayload::Message(message)) => match &message.parts[0] {
                    crate::message::MessagePart::ToolResult { tool_use_id, .. } => {
                        tool_use_id.as_str()
                    }
                    _ => panic!("tool result part"),
                },
                _ => panic!("tool result message"),
            })
            .collect();
        assert_eq!(ids, vec!["slow_id", "fast_id"]);
    }

    struct TextTool {
        name: &'static str,
        output: String,
    }

    impl Tool for TextTool {
        fn name(&self) -> &str {
            self.name
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            let output = self.output.clone();
            Box::pin(async move { Ok(Value::Str(output)) })
        }
    }

    #[tokio::test]
    async fn dispatch_all_preserves_fs_read_pagination_for_full_utf8_reassembly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let expected = "界".repeat(349_525) + "a";
        assert_eq!(expected.len(), 1_048_576);
        tokio::fs::write(&path, &expected).await.unwrap();

        let registry = crate::tool::ToolRegistry::new();
        crate::tools::register_tier_zero(&registry);
        let ctx = authorized_ctx(std::sync::Arc::new(registry))
            .with_session_dir(dir.path().to_path_buf())
            .with_workspace(crate::git_workspace::WorkspaceBinding {
                workspace_id: "stdlib-test".into(),
                repository_root: dir.path().to_path_buf(),
                path: dir.path().to_path_buf(),
                branch: None,
            });
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("read_id".into())),
            ("name".into(), Value::Str("fs.read".into())),
            (
                "intent".into(),
                Value::Str("Read the complete fixture".into()),
            ),
            (
                "input".into(),
                Value::Struct(vec![("path".into(), Value::Host(AtmanPayload::Path(path)))]),
            ),
        ])]);
        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Host(AtmanPayload::Message(message)) = &results[0] else {
            panic!("tool result message");
        };
        let crate::message::MessagePart::ToolResult { content, .. } = &message.parts[0] else {
            panic!("tool result part");
        };
        let envelope: serde_json::Value = serde_json::from_str(content).unwrap();
        let output_id = envelope["output_id"].as_str().unwrap();
        let mut reconstructed = envelope["content"].as_str().unwrap().to_string();
        let mut offset = envelope["next"]["offset"].as_u64().unwrap() as usize;
        while envelope["next"]["has_more"].as_bool().unwrap() || offset < expected.len() {
            let page = ctx
                .output_store
                .as_ref()
                .unwrap()
                .read_bytes(output_id, offset, usize::MAX, ctx.tool_output_budget)
                .unwrap();
            reconstructed.push_str(&page.content);
            if !page.has_more {
                break;
            }
            assert!(page.next_offset > offset);
            offset = page.next_offset;
        }
        assert_eq!(reconstructed.len(), 1_048_576);
        assert_eq!(reconstructed, expected);
    }

    #[tokio::test]
    async fn dispatch_all_returns_the_same_truncated_message_it_emits() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(TextTool {
            name: "text",
            output: "0123456789".repeat(20),
        }));
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let events = crate::event::EventSink::new();
        let mut ctx = ToolCtx::new()
            .with_registry(std::sync::Arc::new(registry))
            .with_stream_tx(stream_tx)
            .with_events(events.clone());
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 32,
            max_bytes: 24,
            max_line_bytes: 24,
        };
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("text_id".into())),
            ("name".into(), Value::Str("text".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);
        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Host(AtmanPayload::Message(returned)) = &results[0] else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };
        assert_eq!(returned, &emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult { content, .. }
                if content.contains("Output truncated")
        ));
        let returned_bytes = match &returned.parts[0] {
            crate::message::MessagePart::ToolResult { content, .. } => content.len() as u64,
            _ => unreachable!(),
        };
        let metrics = events
            .snapshot()
            .into_iter()
            .find_map(|event| match event {
                crate::event::Event::ToolResultMetrics {
                    tool_use_id,
                    raw_bytes,
                    excerpt_bytes,
                    truncated,
                    ..
                } if tool_use_id == "text_id" => Some((raw_bytes, excerpt_bytes, truncated)),
                _ => None,
            })
            .expect("tool result metrics");
        assert!(metrics.0 > 0);
        assert_eq!((metrics.1, metrics.2), (returned_bytes, true));
    }

    #[test]
    fn finish_dispatch_returns_the_budgeted_message_it_emits() {
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let events = crate::event::EventSink::new();
        let mut ctx = ToolCtx::new()
            .with_stream_tx(stream_tx)
            .with_events(events.clone());
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 32,
            max_bytes: 24,
            max_line_bytes: 24,
        };
        let Value::Host(AtmanPayload::Message(returned)) = finish_dispatch(
            &ctx,
            "text_id",
            "text",
            Ok(Value::Str("0123456789".repeat(20))),
        ) else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };

        assert_eq!(returned, emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult { content, .. }
                if content.contains("Output truncated")
        ));
        let excerpt_bytes = match &returned.parts[0] {
            crate::message::MessagePart::ToolResult { content, .. } => content.len() as u64,
            _ => unreachable!(),
        };
        assert!(events.snapshot().into_iter().any(|event| matches!(
            event,
            crate::event::Event::ToolResultMetrics {
                tool_use_id,
                raw_bytes: 200,
                excerpt_bytes: observed,
                truncated: true,
                ..
            } if tool_use_id == "text_id" && observed == excerpt_bytes
        )));
    }

    #[test]
    fn finish_dispatch_preserves_error_flag_and_message_consistency() {
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new().with_stream_tx(stream_tx);
        let Value::Host(AtmanPayload::Message(returned)) = finish_dispatch(
            &ctx,
            "error_id",
            "failing",
            Err(RuntimeError::ToolFailed("expected failure".into())),
        ) else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };

        assert_eq!(returned, emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult {
                content,
                is_error: true,
                ..
            } if content.contains("expected failure")
        ));
    }

    #[test]
    fn finish_dispatch_emits_full_diff_preview_before_result_truncation() {
        let events = crate::event::EventSink::new();
        let mut ctx = ToolCtx::new().with_events(events.clone());
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 1,
            max_bytes: 16,
            max_line_bytes: 16,
        };
        let diff = "-old\n+new\n".repeat(20);
        let Value::Host(AtmanPayload::Message(returned)) = finish_dispatch(
            &ctx,
            "edit_id",
            "fs.edit",
            Ok(Value::Struct(vec![
                (
                    "summary".into(),
                    Value::Str("[fs.edit(example.txt): updated]".into()),
                ),
                ("diff".into(), Value::Str(diff.clone())),
            ])),
        ) else {
            panic!("tool result message");
        };

        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult { content, .. }
                if content.contains("Output truncated")
        ));
        assert!(events.snapshot().iter().any(|event| matches!(
            event,
            crate::event::Event::DiffPreview {
                tool_use_id: Some(tool_use_id),
                unified_diff: Some(preview),
                ..
            } if tool_use_id == "edit_id" && preview == &diff
        )));
    }

    #[tokio::test]
    async fn dispatch_all_returns_the_same_truncated_preflight_error_it_emits() {
        let registry = crate::tool::ToolRegistry::new();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let mut ctx = ToolCtx::new()
            .with_registry(std::sync::Arc::new(registry))
            .with_stream_tx(stream_tx);
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 32,
            max_bytes: 32,
            max_line_bytes: 32,
        };
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("unknown_id".into())),
            ("name".into(), Value::Str("missing".repeat(40))),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Host(AtmanPayload::Message(returned)) = &results[0] else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };

        assert_eq!(returned, &emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult {
                content,
                is_error: true,
                ..
            } if content.contains("Output truncated")
        ));
    }

    #[tokio::test]
    async fn dispatch_all_unknown_tool_finishes_its_workflow_node_with_error() {
        use crate::workflow::{NodeStatus, WorkflowGraph};

        let registry = crate::tool::ToolRegistry::new();
        let run_id = crate::event::FlowRunId::now();
        let run = run_id.0.to_string();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new()
            .with_anchors(None, Some(run_id), None)
            .with_current_node(Some("dispatch_all".into()))
            .with_registry(std::sync::Arc::new(registry))
            .with_stream_tx(stream_tx);
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("unknown_id".into())),
            ("name".into(), Value::Str("missing.tool".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap();

        let mut graph = WorkflowGraph::new(crate::event::TurnId::now());
        graph.apply_stream_frame(&crate::stream::StreamFrame::FlowStart {
            run_id: run.clone(),
            flow_name: "agent_loop".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        graph.apply_stream_frame(&crate::stream::StreamFrame::FlowNodeStart {
            run_id: run.clone(),
            node_id: "dispatch_all".into(),
            kind: crate::nodegraph::NodeKind::ToolCall {
                path: "dispatch_all".into(),
            },
            label: "dispatch_all".into(),
            parent_node_id: None,
        });
        while let Ok(frame) = stream_rx.try_recv() {
            graph.apply_stream_frame(&frame);
        }

        let node = graph
            .find_node(&format!("tool:{run}:unknown_id"))
            .expect("unknown tool node");
        assert_eq!(node.status, NodeStatus::Err);
        assert!(matches!(
            &node.kind,
            crate::workflow::WorkflowNodeKind::ToolCall {
                result_preview: Some(preview),
                ..
            } if preview.contains("unknown tool")
        ));
        let result = node.output_preview.as_deref().unwrap();
        assert!(result.contains("unknown tool"));
        assert!(
            graph
                .root
                .iter()
                .flat_map(|root| &root.children)
                .all(|child| {
                    !matches!(
                        &child.kind,
                        crate::workflow::WorkflowNodeKind::ToolCall { tool_use_id, .. }
                            if tool_use_id == "unknown_id"
                    )
                })
        );
    }

    #[test]
    fn prepare_dispatch_does_not_emit_partial_nodes_for_malformed_batch() {
        let registry = crate::tool::ToolRegistry::new();
        let run_id = crate::event::FlowRunId::now();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new()
            .with_anchors(None, Some(run_id), None)
            .with_current_node(Some("dispatch_all".into()))
            .with_stream_tx(stream_tx);
        let uses = vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("valid_id".into())),
                ("name".into(), Value::Str("missing.tool".into())),
            ]),
            Value::Struct(vec![("name".into(), Value::Str("missing.tool".into()))]),
        ];

        assert!(prepare_dispatch(&uses, &registry, &ctx).is_err());
        assert!(matches!(
            stream_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
