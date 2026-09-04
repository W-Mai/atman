use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use atman_runtime::TranscriptEntry;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::projection::workflow::WorkflowProjection;
use atman_runtime::stream::StreamFrame;
use atman_runtime::workflow::{WorkflowPermissionIdentity, WorkflowPermissionState};

use crate::app::{
    ActivityTotals, Disclosure, FsDetail, FsSearchHit, NoteLevel, OutputItem, ToolCallStatus,
    ToolCallView,
};

#[derive(Debug, Clone)]
pub(crate) struct ToolDisplayMeta {
    pub(crate) name: String,
    pub(crate) call_intent: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) input: Option<serde_json::Value>,
}

impl ToolDisplayMeta {
    pub(crate) fn from_tool_use(
        name: &str,
        input: &serde_json::Value,
        intent: Option<&atman_runtime::message::ToolCallIntent>,
    ) -> Self {
        let command = match name {
            "bash.spawn" | "term.spawn" => input
                .get("cmd")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        };
        let input = matches!(name, "fs.read" | "fs.list" | "fs.grep").then(|| input.clone());
        Self {
            name: name.to_owned(),
            call_intent: intent.map(|intent| intent.as_str().to_owned()),
            command,
            input,
        }
    }
}

pub fn flatten_transcript(entries: &[TranscriptEntry]) -> Vec<OutputItem> {
    flatten_transcript_impl(entries, None)
}

pub fn flatten_transcript_with_output_store(
    entries: &[TranscriptEntry],
    output_store: &atman_runtime::tools::tool_output::OutputStore,
) -> Vec<OutputItem> {
    flatten_transcript_impl(entries, Some(output_store))
}

fn flatten_transcript_impl(
    entries: &[TranscriptEntry],
    output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
) -> Vec<OutputItem> {
    let mut tool_map: HashMap<String, ToolDisplayMeta> = HashMap::new();
    // First pass: build tool_map + collect FlowStart parent links + FlowDone status
    // for transitive closure of spawned flows.
    let mut flow_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut spawned_roots: HashSet<String> = HashSet::new();
    let mut flow_dones: HashMap<String, (bool, bool)> = HashMap::new();
    let mut tool_runs: HashMap<String, Option<String>> = HashMap::new();
    for entry in entries {
        match entry {
            TranscriptEntry::Message { message, .. } => {
                for part in &message.parts {
                    if let MessagePart::ToolUse {
                        id,
                        name,
                        input,
                        intent,
                    } = part
                    {
                        tool_map.insert(
                            id.clone(),
                            ToolDisplayMeta::from_tool_use(name, input, intent.as_ref()),
                        );
                    }
                }
            }
            TranscriptEntry::FlowStart {
                run_id,
                parent_run_id,
                spawned,
                ..
            } => {
                if let Some(parent_run_id) = parent_run_id {
                    flow_children
                        .entry(parent_run_id.clone())
                        .or_default()
                        .push(run_id.clone());
                }
                if *spawned {
                    spawned_roots.insert(run_id.clone());
                }
            }
            TranscriptEntry::FlowDone {
                run_id,
                ok,
                cancelled,
                ..
            } => {
                flow_dones.insert(run_id.clone(), (*ok, *cancelled));
            }
            TranscriptEntry::ToolNode {
                run_id,
                tool_use_id,
                ..
            } => {
                tool_runs
                    .entry(tool_use_id.clone())
                    .and_modify(|existing| {
                        if existing.as_deref() != Some(run_id.as_str()) {
                            *existing = None;
                        }
                    })
                    .or_insert_with(|| Some(run_id.clone()));
            }
            _ => {}
        }
    }
    let mut spawned_root_by_run = spawned_roots
        .iter()
        .map(|root| (root.clone(), root.clone()))
        .collect::<HashMap<_, _>>();
    let mut queue = VecDeque::from_iter(spawned_roots.iter().cloned());
    while let Some(parent) = queue.pop_front() {
        let root = spawned_root_by_run[&parent].clone();
        let Some(children) = flow_children.get(&parent) else {
            continue;
        };
        for child in children {
            if spawned_roots.contains(child) {
                continue;
            }
            if spawned_root_by_run
                .insert(child.clone(), root.clone())
                .is_none()
            {
                queue.push_back(child.clone());
            }
        }
    }
    let spawned_set = spawned_root_by_run.keys().cloned().collect::<HashSet<_>>();
    let find_spawned_root = |rid: &str| spawned_root_by_run.get(rid).cloned();
    let mut llm_models: HashMap<String, String> = HashMap::new();
    for entry in entries {
        if let TranscriptEntry::LlmCall {
            run_id: Some(run_id),
            model,
            ..
        } = entry
            && let Some(root_id) = find_spawned_root(&run_id.0.to_string())
        {
            llm_models.entry(root_id).or_insert_with(|| model.clone());
        }
    }

    let mut out: Vec<OutputItem> = Vec::new();
    let mut current_workflow_idx: Option<usize> = None;
    let mut sub_agent_indices: HashMap<String, usize> = HashMap::new();
    let mut sub_agent_messages: HashMap<String, Vec<Message>> = HashMap::new();
    let mut sub_agent_entries: HashMap<String, Vec<&TranscriptEntry>> = HashMap::new();
    let mut terminal_indices: HashMap<String, Vec<usize>> = HashMap::new();
    let mut workflow_permission_batches: HashMap<
        usize,
        Vec<(
            WorkflowPermissionIdentity,
            atman_runtime::permission_audit::PermissionRequestAudit,
            WorkflowPermissionState,
        )>,
    > = HashMap::new();
    let ensure_panel = |out: &mut Vec<OutputItem>, current: &mut Option<usize>| -> usize {
        if let Some(i) = *current
            && let Some(OutputItem::WorkflowPanel { ended_at: None, .. }) = out.get(i)
        {
            return i;
        }
        let turn_index = out
            .iter()
            .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
            .count();
        out.push(OutputItem::WorkflowPanel {
            turn_index,
            graph: WorkflowProjection::new(atman_runtime::event::TurnId::now()),
            expanded_nodes: HashSet::new(),
            panel_expanded: false,
            started_at: Instant::now(),
            ended_at: None,
            cancelled: false,
        });
        let idx = out.len() - 1;
        *current = Some(idx);
        idx
    };
    let apply_workflow = |out: &mut Vec<OutputItem>,
                          idx: usize,
                          frame: &StreamFrame,
                          ts: Option<chrono::DateTime<chrono::Utc>>| {
        if let Some(OutputItem::WorkflowPanel {
            graph,
            ended_at,
            cancelled,
            ..
        }) = out.get_mut(idx)
        {
            graph.apply_stream_frame_at(frame, ts);
            if let StreamFrame::FlowDone {
                cancelled: flow_cancelled,
                suicide,
                ..
            } = frame
            {
                *ended_at = Some(Instant::now());
                *cancelled = *flow_cancelled || *suicide;
            }
        }
    };
    for entry in entries {
        let entry_run_id = match entry {
            TranscriptEntry::Message { flow_run_id, .. } => flow_run_id.clone(),
            TranscriptEntry::FlowGraph { run_id, .. }
            | TranscriptEntry::FlowStart { run_id, .. }
            | TranscriptEntry::FlowNodeStart { run_id, .. }
            | TranscriptEntry::FlowNodeEnd { run_id, .. }
            | TranscriptEntry::ToolNode { run_id, .. }
            | TranscriptEntry::FlowDone { run_id, .. } => Some(run_id.clone()),
            TranscriptEntry::LlmCall { run_id, .. } => {
                run_id.as_ref().map(|run_id| run_id.0.to_string())
            }
            TranscriptEntry::PermissionRequest { payload, .. } => {
                Some(payload.requesting_run_id.0.to_string())
            }
            TranscriptEntry::PermissionGroup { payload, .. } => match &payload.owner {
                atman_runtime::permission_audit::PermissionGroupAuditOwner::Flow { run_id } => {
                    Some(run_id.0.to_string())
                }
                atman_runtime::permission_audit::PermissionGroupAuditOwner::User { .. }
                | atman_runtime::permission_audit::PermissionGroupAuditOwner::System => None,
            },
            _ => None,
        };
        let spawned_root = entry_run_id.as_deref().and_then(&find_spawned_root);
        if let Some(root_id) = spawned_root.as_ref() {
            sub_agent_entries
                .entry(root_id.clone())
                .or_default()
                .push(entry);
        }
        match entry {
            TranscriptEntry::Message {
                message: msg,
                flow_run_id,
            } => {
                if matches!(msg.role, MessageRole::System)
                    && matches!(msg.parts.as_slice(), [MessagePart::CompactSummary { .. }])
                {
                    if let Some(summary) = parse_compaction_summary(msg) {
                        out.push(summary);
                    }
                    continue;
                }
                if matches!(msg.role, MessageRole::User)
                    && flow_run_id.is_none()
                    && let Some(i) = current_workflow_idx
                    && let Some(OutputItem::WorkflowPanel { ended_at: None, .. }) = out.get(i)
                {
                    // user_msg with flow_run_id=None can be either:
                    // (a) genuine user message starting a new turn, or
                    // (b) session.push(message.user(...)) inside a flow.
                    // We can't distinguish them here, so don't touch the panel.
                    // Panel closing is handled by FlowStart (new root flow) and
                    // FlowDone (flow completion) instead.
                }
                let inferred_tool_run =
                    if matches!(msg.role, MessageRole::Tool) && flow_run_id.is_none() {
                        let results: Vec<_> = msg
                            .parts
                            .iter()
                            .filter_map(|part| match part {
                                MessagePart::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                                _ => None,
                            })
                            .collect();
                        results.first().and_then(|first_id| {
                            let first_run = tool_runs.get(*first_id).and_then(Clone::clone)?;
                            results
                                .iter()
                                .all(|tool_use_id| {
                                    tool_runs.get(*tool_use_id).and_then(Clone::clone)
                                        == Some(first_run.clone())
                                })
                                .then_some(first_run)
                        })
                    } else {
                        None
                    };
                let workflow_run_id = flow_run_id.as_deref().or(inferred_tool_run.as_deref());
                if matches!(msg.role, MessageRole::Assistant | MessageRole::Tool)
                    && workflow_run_id.is_some()
                    && let Some(idx) = current_workflow_idx
                    && let Some(OutputItem::WorkflowPanel { graph, .. }) = out.get_mut(idx)
                {
                    apply_message_to_workflow(graph, msg, workflow_run_id);
                }
                if let Some(root_id) = spawned_root {
                    if !sub_agent_indices.contains_key(&root_id) {
                        let (ok, cancelled) =
                            flow_dones.get(&root_id).cloned().unwrap_or((false, false));
                        let status = if cancelled {
                            "killed".into()
                        } else if ok {
                            "ok".into()
                        } else {
                            "running".into()
                        };
                        let model = llm_models.get(&root_id).cloned().unwrap_or_default();
                        out.push(OutputItem::SubAgentActivity {
                            handle: root_id.clone(),
                            goal: String::new(),
                            child_run_id: root_id.clone(),
                            model,
                            status,
                            output: String::new(),
                            iteration: 0,
                            done: flow_dones.contains_key(&root_id),
                            expanded: false,
                            messages: Vec::new(),
                            workflow_graph: WorkflowProjection::new(
                                atman_runtime::event::TurnId::now(),
                            ),
                            expanded_nodes: HashSet::new(),
                            workflow_expanded: false,
                        });
                        sub_agent_indices.insert(root_id.clone(), out.len() - 1);
                    }
                    sub_agent_messages
                        .entry(root_id)
                        .or_default()
                        .push(msg.clone());
                } else {
                    let first_new_item = out.len();
                    flatten_message_with_output_store(msg, &mut out, &tool_map, output_store);
                    for (item_index, item) in out.iter().enumerate().skip(first_new_item) {
                        if let OutputItem::Terminal { handle, .. } = item {
                            terminal_indices
                                .entry(handle.clone())
                                .or_default()
                                .push(item_index);
                        }
                    }
                }
            }
            TranscriptEntry::ToolTiming {
                tool_use_id,
                elapsed_ms,
            } => {
                let now = Instant::now();
                let elapsed = std::time::Duration::from_millis(*elapsed_ms);
                for item in out.iter_mut().rev() {
                    let OutputItem::ToolDispatch { calls } = item else {
                        continue;
                    };
                    if let Some(call) = calls.iter_mut().find(|call| call.id == *tool_use_id) {
                        call.started_at = now.checked_sub(elapsed).unwrap_or(now);
                        call.ended_at = Some(now);
                        break;
                    }
                }
            }
            TranscriptEntry::ActivitySummary {
                turn,
                session,
                turn_files,
                session_files,
            } => {
                if turn.attempted_calls > 0 || turn.applied_edits > 0 {
                    out.push(OutputItem::ActivitySummary {
                        turn: ActivityTotals::from_summary(turn, turn_files.iter().cloned()),
                        session: ActivityTotals::from_summary(
                            session,
                            session_files.iter().cloned(),
                        ),
                    });
                }
            }
            TranscriptEntry::DiffPreview {
                tool_use_id,
                title,
                old_content,
                new_content,
                unified_diff,
            } => {
                let detail = OutputItem::DiffPreview {
                    title: title.clone(),
                    old_content: old_content.clone(),
                    new_content: new_content.clone(),
                    unified_diff: unified_diff.clone(),
                    expanded: false,
                };
                if tool_use_id
                    .as_deref()
                    .is_none_or(|id| !attach_detail(out.as_mut_slice(), id, detail.clone()))
                {
                    out.push(detail);
                }
            }
            TranscriptEntry::FileEditApplied {
                tool_use_id: Some(tool_use_id),
                path,
                metrics,
                ..
            } => {
                for item in out.iter_mut().rev() {
                    let OutputItem::ToolDispatch { calls } = item else {
                        continue;
                    };
                    if let Some(call) = calls.iter_mut().find(|call| call.id == *tool_use_id) {
                        call.applied_edit = Some((path.clone(), *metrics));
                        break;
                    }
                }
            }
            TranscriptEntry::FileEditApplied { .. } => {}
            TranscriptEntry::CompactionSummary {
                range_start,
                range_end,
                compacted_count,
                before_tokens,
                after_tokens,
                summary,
                ..
            } => {
                if matches!(
                    out.last(),
                    Some(OutputItem::CompactionSummary {
                        phase: atman_runtime::stream::CompactionPhase::Finished,
                        range_start: last_start,
                        range_end: last_end,
                        summary: last_summary,
                        ..
                    }) if *last_start == *range_start
                        && *last_end == *range_end
                        && last_summary == summary
                ) {
                    continue;
                }
                out.push(OutputItem::CompactionSummary {
                    phase: atman_runtime::stream::CompactionPhase::Finished,
                    range_start: *range_start,
                    range_end: *range_end,
                    summary: summary.clone(),
                    before_tokens: *before_tokens,
                    after_tokens: *after_tokens,
                    compacted_count: *compacted_count,
                    disclosure: Disclosure::Summary,
                });
            }
            TranscriptEntry::FlowGraph {
                run_id, graph, ts, ..
            } => {
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowGraph {
                        run_id: run_id.clone(),
                        graph: graph.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                spawned: _,
                ts,
            } => {
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                if parent_run_id.is_none() {
                    current_workflow_idx = None;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowStart {
                        run_id: run_id.clone(),
                        flow_name: flow_name.clone(),
                        parent_run_id: parent_run_id.clone(),
                        parent_node_id: parent_node_id.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowNodeStart {
                run_id,
                node_id,
                kind,
                label,
                parent_node_id,
                ts,
            } => {
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowNodeStart {
                        run_id: run_id.clone(),
                        node_id: node_id.clone(),
                        kind: kind.clone(),
                        label: label.clone(),
                        parent_node_id: parent_node_id.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowNodeEnd {
                run_id,
                node_id,
                status,
                output_preview,
                ts,
            } => {
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowNodeEnd {
                        run_id: run_id.clone(),
                        node_id: node_id.clone(),
                        status: status.clone(),
                        output_preview: output_preview.clone(),
                        parent_node_id: None,
                    },
                    *ts,
                );
            }
            TranscriptEntry::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool_name,
                args_preview,
                call_intent,
                ts,
            } => {
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::ToolNode {
                        run_id: run_id.clone(),
                        parent_node_id: parent_node_id.clone(),
                        tool_use_id: tool_use_id.clone(),
                        tool: tool_name.clone(),
                        args_preview: args_preview.clone(),
                        call_intent: call_intent.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowDone {
                run_id,
                ok,
                cancelled,
                ts,
            } => {
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowDone {
                        run_id: run_id.clone(),
                        flow_name: String::new(),
                        ok: *ok,
                        cancelled: *cancelled,
                        suicide: false,
                    },
                    *ts,
                );
            }
            TranscriptEntry::LlmCall {
                model,
                provider,
                context_call_purpose,
                context_call_scope,
                usage,
                wallclock_ms,
                ttft_ms,
                tokens_per_second,
                run_id,
                node_id,
                ts,
            } => {
                if run_id
                    .as_ref()
                    .is_some_and(|r| spawned_set.contains(&r.0.to_string()))
                {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::LlmCallStats {
                        model: model.clone(),
                        provider: provider.clone(),
                        context_call_purpose: *context_call_purpose,
                        context_call_scope: *context_call_scope,
                        input_tokens: usage.input,
                        output_tokens: usage.output,
                        cache_read: usage.cached_input,
                        cache_write: usage.cache_write,
                        ttft_ms: ttft_ms.unwrap_or(0),
                        tokens_per_second: tokens_per_second.unwrap_or(0.0),
                        wallclock_ms: *wallclock_ms,
                        run_id: run_id.as_ref().map(|r| r.0.to_string()),
                        node_id: node_id.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::PermissionRequest {
                identity,
                payload,
                state,
            } => {
                let run_id = payload.requesting_run_id.0.to_string();
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                workflow_permission_batches
                    .entry(panel_idx)
                    .or_default()
                    .push((identity.clone(), payload.as_ref().clone(), *state));
            }
            TranscriptEntry::PermissionGroup { payload, resolved } => {
                let Some(run_id) = (match &payload.owner {
                    atman_runtime::permission_audit::PermissionGroupAuditOwner::Flow { run_id } => {
                        Some(run_id.0.to_string())
                    }
                    atman_runtime::permission_audit::PermissionGroupAuditOwner::User { .. }
                    | atman_runtime::permission_audit::PermissionGroupAuditOwner::System => None,
                }) else {
                    continue;
                };
                if spawned_set.contains(run_id.as_str()) {
                    continue;
                }
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                if let Some(OutputItem::WorkflowPanel { graph, .. }) = out.get_mut(panel_idx) {
                    graph.apply_permission_group(payload, *resolved);
                }
            }
            TranscriptEntry::TerminalFinalState { handle, screen } => {
                for item_index in terminal_indices.get(handle).into_iter().flatten() {
                    if let Some(OutputItem::Terminal {
                        screen: current, ..
                    }) = out.get_mut(*item_index)
                    {
                        *current = screen.clone();
                    }
                }
            }
            TranscriptEntry::MermaidDiagram { source } => {
                out.push(OutputItem::MermaidDiagram {
                    source: source.clone(),
                });
            }
        }
    }
    for (panel_idx, requests) in workflow_permission_batches {
        if let Some(OutputItem::WorkflowPanel { graph, .. }) = out.get_mut(panel_idx) {
            graph.apply_permission_requests(requests);
        }
    }
    // Fill in SubAgentActivity items with collected messages.
    for (root_id, msgs) in sub_agent_messages {
        if let Some(&idx) = sub_agent_indices.get(&root_id)
            && let Some(OutputItem::SubAgentActivity {
                messages,
                output,
                goal,
                workflow_graph,
                ..
            }) = out.get_mut(idx)
        {
            *messages = msgs.clone();
            *output = msgs
                .iter()
                .filter(|m| matches!(m.role, MessageRole::Assistant))
                .map(|m| m.text_concat())
                .collect::<Vec<_>>()
                .join("\n");
            if goal.is_empty()
                && let Some(first_user) = msgs.iter().find(|m| matches!(m.role, MessageRole::User))
            {
                *goal = first_user.text_concat();
            }
            // Rebuild workflow_graph from transcript entries belonging to this
            // sub-agent (identified by transitive closure of spawned flows).
            let mut permission_requests = Vec::new();
            for entry in sub_agent_entries.remove(&root_id).unwrap_or_default() {
                match entry {
                    TranscriptEntry::PermissionRequest {
                        identity,
                        payload,
                        state,
                    } => {
                        permission_requests.push((
                            identity.clone(),
                            payload.as_ref().clone(),
                            *state,
                        ));
                        continue;
                    }
                    TranscriptEntry::PermissionGroup { payload, resolved } => {
                        workflow_graph.apply_permission_group(payload, *resolved);
                        continue;
                    }
                    _ => {}
                }
                let (frame, ts) = match entry {
                    TranscriptEntry::FlowStart {
                        run_id,
                        flow_name,
                        parent_run_id,
                        parent_node_id,
                        ts,
                        ..
                    } => (
                        StreamFrame::FlowStart {
                            run_id: run_id.clone(),
                            flow_name: flow_name.clone(),
                            parent_run_id: parent_run_id.clone(),
                            parent_node_id: parent_node_id.clone(),
                        },
                        *ts,
                    ),
                    TranscriptEntry::FlowNodeStart {
                        run_id,
                        node_id,
                        kind,
                        label,
                        parent_node_id,
                        ts,
                    } => (
                        StreamFrame::FlowNodeStart {
                            run_id: run_id.clone(),
                            node_id: node_id.clone(),
                            kind: kind.clone(),
                            label: label.clone(),
                            parent_node_id: parent_node_id.clone(),
                        },
                        *ts,
                    ),
                    TranscriptEntry::FlowNodeEnd {
                        run_id,
                        node_id,
                        status,
                        output_preview,
                        ts,
                    } => (
                        StreamFrame::FlowNodeEnd {
                            run_id: run_id.clone(),
                            node_id: node_id.clone(),
                            status: status.clone(),
                            output_preview: output_preview.clone(),
                            parent_node_id: None,
                        },
                        *ts,
                    ),
                    TranscriptEntry::ToolNode {
                        run_id,
                        parent_node_id,
                        tool_use_id,
                        tool_name,
                        args_preview,
                        call_intent,
                        ts,
                    } => (
                        StreamFrame::ToolNode {
                            run_id: run_id.clone(),
                            parent_node_id: parent_node_id.clone(),
                            tool_use_id: tool_use_id.clone(),
                            tool: tool_name.clone(),
                            args_preview: args_preview.clone(),
                            call_intent: call_intent.clone(),
                        },
                        *ts,
                    ),
                    TranscriptEntry::FlowDone {
                        run_id,
                        ok,
                        cancelled,
                        ts,
                    } => (
                        StreamFrame::FlowDone {
                            run_id: run_id.clone(),
                            flow_name: String::new(),
                            ok: *ok,
                            cancelled: *cancelled,
                            suicide: false,
                        },
                        *ts,
                    ),
                    TranscriptEntry::LlmCall {
                        model,
                        provider,
                        context_call_purpose,
                        context_call_scope,
                        usage,
                        wallclock_ms,
                        ttft_ms,
                        tokens_per_second,
                        run_id,
                        node_id,
                        ts,
                    } => (
                        StreamFrame::LlmCallStats {
                            model: model.clone(),
                            provider: provider.clone(),
                            context_call_purpose: *context_call_purpose,
                            context_call_scope: *context_call_scope,
                            input_tokens: usage.input,
                            output_tokens: usage.output,
                            cache_read: usage.cached_input,
                            cache_write: usage.cache_write,
                            ttft_ms: ttft_ms.unwrap_or(0),
                            tokens_per_second: tokens_per_second.unwrap_or(0.0),
                            wallclock_ms: *wallclock_ms,
                            run_id: run_id.as_ref().map(|r| r.0.to_string()),
                            node_id: node_id.clone(),
                        },
                        *ts,
                    ),
                    TranscriptEntry::Message {
                        message,
                        flow_run_id: Some(frid),
                    } => {
                        let frame = match message.role {
                            MessageRole::Assistant => StreamFrame::AssistantMsg {
                                flow_run_id: Some(frid.clone()),
                                message: message.clone(),
                            },
                            MessageRole::Tool => StreamFrame::ToolResultMsg {
                                flow_run_id: Some(frid.clone()),
                                message: message.clone(),
                            },
                            _ => continue,
                        };
                        (frame, None)
                    }
                    _ => continue,
                };
                workflow_graph.apply_stream_frame_at(&frame, ts);
            }
            if !permission_requests.is_empty() {
                workflow_graph.apply_permission_requests(permission_requests);
            }
        }
    }
    // A restored session cannot have a flow still running.
    for item in out.iter_mut() {
        if let OutputItem::WorkflowPanel {
            graph, ended_at: e, ..
        } = item
        {
            graph.interrupt_pending_permissions();
            if e.is_none() {
                *e = Some(Instant::now());
            }
        }
        if let OutputItem::SubAgentActivity {
            done,
            status,
            workflow_graph,
            ..
        } = item
        {
            workflow_graph.interrupt_pending_permissions();
            if !*done {
                *done = true;
                *status = "interrupted".into();
            }
        }
    }
    dedup_by_handle(&mut out);
    out
}

/// Remove earlier Terminal/Bash items that share a handle with a later one.
/// When the later item has empty output but the earlier one has content
/// (last call was bash.spawn with no output, but a prior bash.output had
/// the real result), keep the one with content.
pub(crate) fn dedup_by_handle(out: &mut Vec<OutputItem>) {
    let mut winners: HashMap<String, usize> = HashMap::new();
    for current in 0..out.len() {
        if let Some(handle) = out[current].handle().map(str::to_owned) {
            if let Some(&previous) = winners.get(&handle) {
                let previous_has_content = bash_has_content(&out[previous]);
                let current_has_content = bash_has_content(&out[current]);
                if previous_has_content && !current_has_content {
                    let current_metadata = execution_metadata(&out[current]);
                    inherit_execution_metadata(&mut out[previous], current_metadata);
                    continue;
                }
                let previous_metadata = execution_metadata(&out[previous]);
                inherit_execution_metadata(&mut out[current], previous_metadata);
            } else {
                winners.insert(handle, current);
                continue;
            }
            winners.insert(handle, current);
        }
    }
    let winner_indices = winners.into_values().collect::<HashSet<_>>();
    let mut original_index = 0;
    out.retain(|item| {
        let keep = item.handle().is_none() || winner_indices.contains(&original_index);
        original_index += 1;
        keep
    });
}

fn execution_metadata(item: &OutputItem) -> (Option<String>, Option<String>) {
    match item {
        OutputItem::Bash { title, command, .. } | OutputItem::Terminal { title, command, .. } => {
            (title.clone(), command.clone())
        }
        _ => (None, None),
    }
}

fn inherit_execution_metadata(
    item: &mut OutputItem,
    (inherited_title, inherited_command): (Option<String>, Option<String>),
) {
    match item {
        OutputItem::Bash { title, command, .. } | OutputItem::Terminal { title, command, .. } => {
            if title.is_none() {
                *title = inherited_title;
            }
            if command.is_none() {
                *command = inherited_command;
            }
        }
        _ => {}
    }
}

fn bash_has_content(item: &OutputItem) -> bool {
    match item {
        OutputItem::Bash { output, .. } => !output.is_empty(),
        OutputItem::Terminal { screen, .. } => !screen.cells.is_empty(),
        _ => true,
    }
}

fn apply_message_to_workflow(
    graph: &mut WorkflowProjection,
    msg: &Message,
    flow_run_id: Option<&str>,
) {
    match msg.role {
        MessageRole::Assistant => {
            graph.apply_stream_frame(&StreamFrame::AssistantMsg {
                flow_run_id: flow_run_id.map(String::from),
                message: msg.clone(),
            });
        }
        MessageRole::Tool => {
            graph.apply_stream_frame(&StreamFrame::ToolResultMsg {
                flow_run_id: flow_run_id.map(String::from),
                message: msg.clone(),
            });
        }
        _ => {}
    }
}

pub(crate) fn flatten_message(
    msg: &Message,
    out: &mut Vec<OutputItem>,
    tool_map: &HashMap<String, ToolDisplayMeta>,
) {
    flatten_message_with_output_store(msg, out, tool_map, None);
}

pub(crate) fn flatten_message_with_output_store(
    msg: &Message,
    out: &mut Vec<OutputItem>,
    tool_map: &HashMap<String, ToolDisplayMeta>,
    output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
) {
    match msg.role {
        MessageRole::User => {
            let text = msg.text_concat();
            if !text.trim().is_empty() {
                out.push(OutputItem::UserTurn { text });
            }
        }
        MessageRole::Assistant => {
            let calls = msg
                .parts
                .iter()
                .filter_map(|part| {
                    let MessagePart::ToolUse {
                        id,
                        name,
                        input,
                        intent,
                    } = part
                    else {
                        return None;
                    };
                    Some(ToolCallView {
                        id: id.clone(),
                        tool: name.clone(),
                        intent: intent
                            .as_ref()
                            .map(|intent| intent.as_str().to_owned())
                            .unwrap_or_else(|| name.clone()),
                        input: input.clone(),
                        status: ToolCallStatus::Running,
                        disclosure: Disclosure::Summary,
                        detail: None,
                        draft_index: None,
                        draft_preview: Default::default(),
                        applied_edit: None,
                        started_at: Instant::now(),
                        ended_at: None,
                    })
                })
                .collect::<Vec<_>>();
            for part in &msg.parts {
                match part {
                    MessagePart::Thinking { thinking, .. } => {
                        if !thinking.is_empty() {
                            out.push(OutputItem::Thinking {
                                text: thinking.clone(),
                                done: true,
                                disclosure: Disclosure::Summary,
                                retried: false,
                            });
                        }
                    }
                    MessagePart::Text { text } => {
                        out.push(OutputItem::AssistantMd {
                            md: text.clone(),
                            streaming: false,
                            retried: false,
                        });
                    }
                    _ => {}
                }
            }
            if !calls.is_empty() {
                out.push(OutputItem::ToolDispatch { calls });
            }
        }
        MessageRole::Tool => {
            for part in &msg.parts {
                if let MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = part
                {
                    let detail = restore_tool_item_with_output_store(
                        tool_map.get(tool_use_id),
                        content,
                        *is_error,
                        output_store,
                    );
                    if !finish_restored_call(
                        out.as_mut_slice(),
                        tool_use_id,
                        *is_error,
                        detail.clone(),
                    ) && let Some(item) = detail
                    {
                        out.push(item);
                    }
                }
            }
        }
        MessageRole::System => {
            if let Some(summary) = parse_compaction_summary(msg) {
                out.push(summary);
            }
        }
    }
}

fn attach_detail(items: &mut [OutputItem], tool_use_id: &str, detail: OutputItem) -> bool {
    for item in items.iter_mut().rev() {
        let OutputItem::ToolDispatch { calls } = item else {
            continue;
        };
        if let Some(call) = calls.iter_mut().find(|call| call.id == tool_use_id) {
            call.detail = Some(Box::new(detail));
            return true;
        }
    }
    false
}

fn finish_restored_call(
    items: &mut [OutputItem],
    tool_use_id: &str,
    is_error: bool,
    detail: Option<OutputItem>,
) -> bool {
    for item in items.iter_mut().rev() {
        let OutputItem::ToolDispatch { calls } = item else {
            continue;
        };
        if let Some(call) = calls.iter_mut().find(|call| call.id == tool_use_id) {
            call.status = if is_error {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Ok
            };
            call.ended_at = Some(Instant::now());
            if call.detail.is_none() {
                call.detail = detail.map(Box::new);
            }
            return true;
        }
    }
    false
}

fn strip_log_prefixes(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("[out] ") {
                rest
            } else if let Some(rest) = line.strip_prefix("[err] ") {
                rest
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fs_input_string(tool_meta: &ToolDisplayMeta, key: &str) -> Option<String> {
    tool_meta
        .input
        .as_ref()?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

fn parse_fs_read_slice(content: &str) -> Option<(String, usize, usize, String)> {
    let rest = content.strip_prefix("[fs.read(")?;
    let (header, body) = rest.split_once("]\n")?;
    let (path, range) = header.rsplit_once("): lines ")?;
    let (range, total) = range.split_once(" of ")?;
    let (start, _) = range.split_once('-')?;
    Some((
        path.to_owned(),
        start.parse().ok()?,
        total.parse().ok()?,
        body.to_owned(),
    ))
}

fn fs_read_requested_slice(tool_meta: &ToolDisplayMeta) -> bool {
    let Some(input) = tool_meta
        .input
        .as_ref()
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    input
        .get("offset")
        .is_some_and(serde_json::Value::is_number)
        || input.get("limit").is_some_and(serde_json::Value::is_number)
        || input
            .get("anchor")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|anchor| !anchor.is_empty())
}

fn parse_fs_read_envelope<'a>(
    content: &str,
    value: Option<&'a serde_json::Value>,
    output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
) -> Option<(&'a str, usize)> {
    if !output_store?.validates_pagination_envelope(content) {
        return None;
    }
    let object = value?.as_object()?;
    let expected = [
        "content",
        "truncated",
        "total_lines",
        "total_bytes",
        "output_id",
        "next",
    ];
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return None;
    }
    let content = object.get("content")?.as_str()?;
    if !object.get("truncated")?.as_bool()? {
        return None;
    }
    let total_lines = usize::try_from(object.get("total_lines")?.as_u64()?).ok()?;
    let total_bytes = usize::try_from(object.get("total_bytes")?.as_u64()?).ok()?;
    let output_id = object.get("output_id")?.as_str()?;
    let suffix = output_id.strip_prefix("out_")?;
    if suffix.len() != 32 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let next = object.get("next")?.as_object()?;
    if next.len() != 3
        || next.get("mode")?.as_str()? != "bytes"
        || !next.get("has_more")?.as_bool()?
    {
        return None;
    }
    let offset = usize::try_from(next.get("offset")?.as_u64()?).ok()?;
    (total_bytes > content.len() && offset < total_bytes).then_some((content, total_lines))
}

fn parse_string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn restore_fs_item(
    tool_meta: &ToolDisplayMeta,
    content: &str,
    is_error: bool,
    output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
) -> Option<OutputItem> {
    if !matches!(tool_meta.name.as_str(), "fs.read" | "fs.list" | "fs.grep") {
        return None;
    }
    let requested_path = fs_input_string(tool_meta, "path");
    if is_error {
        return Some(OutputItem::FsDetail {
            view: FsDetail::Raw {
                tool: tool_meta.name.clone(),
                path: requested_path,
                content: content.to_owned(),
                is_error: true,
            },
            expanded: false,
        });
    }

    let parsed = serde_json::from_str::<serde_json::Value>(content).ok();
    let view = match tool_meta.name.as_str() {
        "fs.read" => {
            let (wire_content, truncated, envelope_total) =
                if let Some((wire_content, total_lines)) =
                    parse_fs_read_envelope(content, parsed.as_ref(), output_store)
                {
                    (wire_content, true, Some(total_lines))
                } else {
                    (content, false, None)
                };
            if fs_read_requested_slice(tool_meta)
                && let Some((header_path, start_line, total_lines, body)) =
                    parse_fs_read_slice(wire_content)
            {
                FsDetail::Read {
                    path: header_path,
                    content: body,
                    start_line,
                    total_lines: Some(total_lines),
                    truncated,
                }
            } else {
                FsDetail::Read {
                    path: requested_path.unwrap_or_else(|| "file".to_string()),
                    content: wire_content.to_owned(),
                    start_line: 1,
                    total_lines: envelope_total.or_else(|| Some(wire_content.lines().count())),
                    truncated,
                }
            }
        }
        "fs.list" => match parse_string_array(parsed.as_ref()) {
            Some(entries) => FsDetail::List {
                path: requested_path.unwrap_or_else(|| ".".to_string()),
                entries,
            },
            None => FsDetail::Raw {
                tool: tool_meta.name.clone(),
                path: requested_path,
                content: content.to_owned(),
                is_error: false,
            },
        },
        "fs.grep" => {
            let hits = parsed
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .and_then(|hits| {
                    hits.iter()
                        .map(|hit| {
                            Some(FsSearchHit {
                                file: hit.get("file")?.as_str()?.to_owned(),
                                line: usize::try_from(hit.get("line")?.as_u64()?).ok()?,
                                before: parse_string_array(hit.get("before"))?,
                                matched: hit.get("match")?.as_str()?.to_owned(),
                                after: parse_string_array(hit.get("after"))?,
                            })
                        })
                        .collect::<Option<Vec<_>>>()
                });
            match hits {
                Some(hits) => FsDetail::Grep {
                    path: requested_path.unwrap_or_else(|| ".".to_string()),
                    pattern: fs_input_string(tool_meta, "pattern").unwrap_or_default(),
                    hits,
                },
                None => FsDetail::Raw {
                    tool: tool_meta.name.clone(),
                    path: requested_path,
                    content: content.to_owned(),
                    is_error: false,
                },
            }
        }
        _ => unreachable!("filesystem tool name checked above"),
    };
    Some(OutputItem::FsDetail {
        view,
        expanded: false,
    })
}

#[cfg(test)]
fn restore_tool_item(
    tool_meta: Option<&ToolDisplayMeta>,
    content: &str,
    is_error: bool,
) -> Option<OutputItem> {
    restore_tool_item_with_output_store(tool_meta, content, is_error, None)
}

pub(crate) fn restore_tool_item_with_output_store(
    tool_meta: Option<&ToolDisplayMeta>,
    content: &str,
    is_error: bool,
    output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
) -> Option<OutputItem> {
    let tool_name = tool_meta.map(|meta| meta.name.as_str()).unwrap_or("");
    let title = tool_meta.and_then(|meta| meta.call_intent.clone());
    if let Some(item) =
        tool_meta.and_then(|meta| restore_fs_item(meta, content, is_error, output_store))
    {
        return Some(item);
    }
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok()?;
    if tool_name.starts_with("bash.") {
        let handle = parsed
            .get("handle")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let output = parsed
            .get("output")
            .and_then(|v| v.as_str())
            .map(strip_log_prefixes)
            .or_else(|| {
                parsed.get("log_path").and_then(|v| v.as_str()).map(|p| {
                    if !std::path::Path::new(p).exists() {
                        return format!("[log file removed: {p}]\n\n{content}");
                    }
                    let raw = std::fs::read_to_string(p).unwrap_or_default();
                    strip_log_prefixes(&raw)
                })
            })
            .unwrap_or_default();
        Some(OutputItem::Bash {
            handle,
            title,
            command: tool_meta.and_then(|meta| meta.command.clone()),
            output,
            done: true,
            expanded: false,
        })
    } else if tool_name.starts_with("term.") || tool_name == "terminal" {
        use atman_runtime::tools::term::{
            DEFAULT_COLS, DEFAULT_ROWS, TerminalCell, TerminalScreen,
        };
        let handle = parsed
            .get("handle")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let rows = parsed
            .get("rows")
            .and_then(|v| v.as_i64())
            .map(|v| v as u16)
            .unwrap_or(DEFAULT_ROWS);
        let cols = parsed
            .get("cols")
            .and_then(|v| v.as_i64())
            .map(|v| v as u16)
            .unwrap_or(DEFAULT_COLS);
        let text = parsed.get("text").and_then(|v| v.as_str()).unwrap_or("");

        // TerminalFinalState events will overwrite this with the full cell grid.
        let mut cells = vec![TerminalCell::default(); (rows as usize) * (cols as usize)];
        for (row, line) in text.lines().enumerate() {
            if row >= rows as usize {
                break;
            }
            let row_start = row * cols as usize;
            for (col, ch) in line.chars().enumerate() {
                if col >= cols as usize {
                    break;
                }
                cells[row_start + col] = TerminalCell {
                    chars: ch.to_string(),
                    ..Default::default()
                };
            }
        }

        Some(OutputItem::Terminal {
            handle,
            title,
            command: tool_meta.and_then(|meta| meta.command.clone()),
            screen: TerminalScreen {
                rows,
                cols,
                cells,
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: text.as_bytes().to_vec(),
            mode: crate::app::TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        })
    } else if tool_name.starts_with("fs.edit")
        || tool_name.starts_with("fs.write")
        || tool_name.starts_with("hunk.")
    {
        let diff = parsed.get("diff").and_then(|v| v.as_str())?;
        if diff.is_empty() {
            return None;
        }
        Some(OutputItem::DiffPreview {
            title: parsed
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(tool_name)
                .to_string(),
            old_content: None,
            new_content: None,
            unified_diff: Some(diff.to_string()),
            expanded: false,
        })
    } else if is_error {
        Some(OutputItem::SystemNote {
            text: format!("{tool_name} error: {content}"),
            level: NoteLevel::Warn,
        })
    } else {
        None
    }
}

fn parse_compaction_summary(msg: &Message) -> Option<OutputItem> {
    let footer = atman_runtime::compaction::find_compact_summaries(std::slice::from_ref(msg))
        .into_iter()
        .next()?;
    let body = msg.text_concat();
    Some(OutputItem::CompactionSummary {
        phase: atman_runtime::stream::CompactionPhase::Finished,
        range_start: footer.seq_start as usize,
        range_end: footer.seq_end as usize,
        summary: body,
        before_tokens: 0,
        after_tokens: 0,
        compacted_count: footer.count,
        disclosure: Disclosure::Summary,
    })
}

pub fn flatten_messages(messages: &[Message]) -> Vec<OutputItem> {
    let tool_map = messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| {
            let MessagePart::ToolUse {
                id,
                name,
                input,
                intent,
            } = part
            else {
                return None;
            };
            Some((
                id.clone(),
                ToolDisplayMeta::from_tool_use(name, input, intent.as_ref()),
            ))
        })
        .collect::<HashMap<_, _>>();
    let mut out: Vec<OutputItem> = Vec::new();
    for msg in messages {
        flatten_message(msg, &mut out, &tool_map);
    }
    dedup_by_handle(&mut out);
    out
}

pub fn history_note(item_count: usize, message_count: usize) -> Option<OutputItem> {
    if item_count == 0 {
        return None;
    }
    Some(OutputItem::SystemNote {
        text: format!(
            "resumed with {message_count} prior message(s), {item_count} item(s) restored"
        ),
        level: NoteLevel::Info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::event::{FlowNodeStatus, FlowRunId, TurnId};

    fn approved_permission(run_id: FlowRunId, tool_use_id: &str) -> TranscriptEntry {
        let request_id = atman_runtime::permission::PermissionRequestId::now();
        TranscriptEntry::PermissionRequest {
            identity: WorkflowPermissionIdentity::Canonical {
                request_id: request_id.clone(),
            },
            payload: Box::new(atman_runtime::permission_audit::PermissionRequestAudit {
                request_id: Some(request_id),
                revision: 1,
                session_id: "session".into(),
                requesting_run_id: run_id.clone(),
                parent_run_id: None,
                root_run_id: run_id,
                tool_use_id: tool_use_id.into(),
                tool: "fs.read".into(),
                call_intent: None,
                tier: atman_runtime::tool::Tier::Zero,
                execution_boundary: Default::default(),
                provenance: Default::default(),
                target: atman_runtime::permission_audit::PermissionAuditTarget::User,
                group_ids: Vec::new(),
                policy: atman_runtime::permission_audit::PermissionPolicyReference {
                    snapshot_id: "snapshot".into(),
                    rule_id: "rule".into(),
                },
                escalation_path: Vec::new(),
                decision_id: Some("decision".into()),
                actor: None,
                scope: None,
                reason: None,
                at: chrono::Utc::now(),
            }),
            state: WorkflowPermissionState::Approved,
        }
    }

    fn assistant(parts: Vec<MessagePart>) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts,
            turn_id: TurnId::now(),
            origin: atman_runtime::message::MessageOrigin::User,
        }
    }

    fn user(text: &str) -> Message {
        Message::user_text(TurnId::now(), text)
    }

    fn tool_detail<'a>(items: &'a [OutputItem], tool_use_id: &str) -> Option<&'a OutputItem> {
        items.iter().find_map(|item| {
            let OutputItem::ToolDispatch { calls } = item else {
                return None;
            };
            calls
                .iter()
                .find(|call| call.id == tool_use_id)
                .and_then(|call| call.detail.as_deref())
        })
    }

    #[test]
    fn user_message_becomes_turn() {
        let out = flatten_messages(&[user("hi")]);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OutputItem::UserTurn { .. }));
    }

    #[test]
    fn assistant_text_becomes_markdown_item() {
        let msgs = vec![assistant(vec![MessagePart::Text {
            text: "hello".into(),
        }])];
        let out = flatten_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OutputItem::AssistantMd { .. }));
    }

    #[test]
    fn tool_use_and_tool_result_parts_form_one_dispatch_item() {
        use serde_json::json;
        let msgs = vec![
            assistant(vec![MessagePart::ToolUse {
                id: "toolu_1".into(),
                name: "fs.read".into(),
                input: json!({}),
                intent: None,
            }]),
            Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "12 bytes".into(),
                    is_error: false,
                }],
                turn_id: TurnId::now(),
                origin: atman_runtime::message::MessageOrigin::User,
            },
        ];
        let out = flatten_messages(&msgs);
        assert!(matches!(
            out.as_slice(),
            [OutputItem::ToolDispatch { calls }]
                if calls.len() == 1
                    && calls[0].id == "toolu_1"
                    && calls[0].status == ToolCallStatus::Ok
                    && matches!(calls[0].detail.as_deref(), Some(OutputItem::FsDetail { .. }))
        ));
    }

    #[test]
    fn filesystem_projection_decodes_read_list_and_grep_results() {
        let read = ToolDisplayMeta::from_tool_use(
            "fs.read",
            &serde_json::json!({"path": "src/lib.rs", "offset": 4, "limit": 2}),
            None,
        );
        let item = restore_tool_item(
            Some(&read),
            "[fs.read(src/lib.rs): lines 4-5 of 9]\nfn alpha() {}\nfn beta() {}\n",
            false,
        )
        .unwrap();
        assert!(matches!(
            item,
            OutputItem::FsDetail {
                view: FsDetail::Read {
                    ref path,
                    start_line: 4,
                    total_lines: Some(9),
                    ref content,
                    truncated: false,
                },
                expanded: false,
            } if path == "src/lib.rs" && content.contains("fn beta")
        ));

        let list =
            ToolDisplayMeta::from_tool_use("fs.list", &serde_json::json!({"path": "src"}), None);
        assert!(matches!(
            restore_tool_item(Some(&list), r#"["src/app.rs","src/lib.rs"]"#, false),
            Some(OutputItem::FsDetail {
                view: FsDetail::List { path, entries },
                ..
            }) if path == "src" && entries == ["src/app.rs", "src/lib.rs"]
        ));

        let grep = ToolDisplayMeta::from_tool_use(
            "fs.grep",
            &serde_json::json!({"path": "src", "pattern": "render_.*"}),
            None,
        );
        let result = serde_json::json!([{
            "file": "src/output.rs",
            "line": 42,
            "before": ["fn before() {}"],
            "match": "fn render_item() {}",
            "after": ["fn after() {}"]
        }])
        .to_string();
        assert!(matches!(
            restore_tool_item(Some(&grep), &result, false),
            Some(OutputItem::FsDetail {
                view: FsDetail::Grep { path, pattern, hits },
                ..
            }) if path == "src"
                && pattern == "render_.*"
                && hits.len() == 1
                && hits[0].line == 42
                && hits[0].matched == "fn render_item() {}"
        ));
    }

    #[test]
    fn filesystem_projection_distinguishes_json_source_from_truncation_envelope() {
        let meta = ToolDisplayMeta::from_tool_use(
            "fs.read",
            &serde_json::json!({"path": "fixture.json"}),
            None,
        );
        let source = r#"{"content":"source value","truncated":true}"#;
        assert!(matches!(
            restore_tool_item(Some(&meta), source, false),
            Some(OutputItem::FsDetail {
                view: FsDetail::Read { content, truncated: false, .. },
                ..
            }) if content == source
        ));

        let dir = tempfile::tempdir().unwrap();
        let output_store = atman_runtime::tools::tool_output::OutputStore::at(dir.path());
        let full = "first\nsecond\nthird\n";
        let output_id = output_store.register("fs_read", full).unwrap();
        let envelope = serde_json::json!({
            "content": "first\nsecond\n",
            "truncated": true,
            "total_lines": 3,
            "total_bytes": full.len(),
            "output_id": output_id,
            "next": {"mode": "bytes", "offset": 13, "has_more": true}
        })
        .to_string();
        assert!(matches!(
            restore_tool_item_with_output_store(
                Some(&meta),
                &envelope,
                false,
                Some(&output_store),
            ),
            Some(OutputItem::FsDetail {
                view: FsDetail::Read {
                    content,
                    total_lines: Some(3),
                    truncated: true,
                    ..
                },
                ..
            }) if content == "first\nsecond\n"
        ));
    }

    #[test]
    fn filesystem_projection_only_decodes_slice_headers_requested_by_the_call() {
        let plain = ToolDisplayMeta::from_tool_use(
            "fs.read",
            &serde_json::json!({"path": "fixture.txt"}),
            None,
        );
        let source = "[fs.read(not-a-protocol): lines 4-5 of 9]\nkept verbatim\n";
        assert!(matches!(
            restore_tool_item(Some(&plain), source, false),
            Some(OutputItem::FsDetail {
                view: FsDetail::Read {
                    path,
                    content,
                    start_line: 1,
                    ..
                },
                ..
            }) if path == "fixture.txt" && content == source
        ));

        let sliced = ToolDisplayMeta::from_tool_use(
            "fs.read",
            &serde_json::json!({"path": "fixture.txt", "limit": 2}),
            None,
        );
        assert!(matches!(
            restore_tool_item(Some(&sliced), source, false),
            Some(OutputItem::FsDetail {
                view: FsDetail::Read {
                    path,
                    content,
                    start_line: 4,
                    total_lines: Some(9),
                    ..
                },
                ..
            }) if path == "not-a-protocol" && content == "kept verbatim\n"
        ));
    }

    #[test]
    fn filesystem_projection_rejects_forged_truncation_envelopes() {
        let meta = ToolDisplayMeta::from_tool_use(
            "fs.read",
            &serde_json::json!({"path": "fixture.json"}),
            None,
        );
        let dir = tempfile::tempdir().unwrap();
        let output_store = atman_runtime::tools::tool_output::OutputStore::at(dir.path());
        let source = serde_json::json!({
            "content": "not an envelope",
            "truncated": true,
            "total_lines": 20,
            "total_bytes": 200,
            "output_id": "out_0123456789abcdef0123456789abcdef",
            "next": {"mode": "bytes", "offset": 13, "has_more": true}
        })
        .to_string();
        assert!(matches!(
            restore_tool_item_with_output_store(
                Some(&meta),
                &source,
                false,
                Some(&output_store),
            ),
            Some(OutputItem::FsDetail {
                view: FsDetail::Read {
                    content,
                    truncated: false,
                    ..
                },
                ..
            }) if content == source
        ));
    }

    #[test]
    fn filesystem_projection_keeps_non_json_errors_visible() {
        let meta = ToolDisplayMeta::from_tool_use(
            "fs.list",
            &serde_json::json!({"path": "/missing"}),
            None,
        );
        assert!(matches!(
            restore_tool_item(Some(&meta), "permission denied", true),
            Some(OutputItem::FsDetail {
                view: FsDetail::Raw {
                    path: Some(path),
                    content,
                    is_error: true,
                    ..
                },
                ..
            }) if path == "/missing" && content == "permission denied"
        ));
    }

    #[test]
    fn replay_restores_tool_duration_and_activity_panel() {
        let tool_use_id = "toolu_1";
        let summary = atman_runtime::activity::ActivitySummary {
            attempted_calls: 1,
            completed_calls: 1,
            failed_calls: 0,
            applied_edits: 1,
            files: 1,
            hunks: 2,
            insertions: 7,
            deletions: 3,
        };
        let entries = vec![
            TranscriptEntry::Message {
                message: assistant(vec![MessagePart::ToolUse {
                    id: tool_use_id.into(),
                    name: "fs.edit".into(),
                    input: serde_json::json!({"path": "/repo/src/lib.rs"}),
                    intent: None,
                }]),
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content: "null".into(),
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
            TranscriptEntry::ToolTiming {
                tool_use_id: tool_use_id.into(),
                elapsed_ms: 1_500,
            },
            TranscriptEntry::ActivitySummary {
                turn: summary.clone(),
                session: summary,
                turn_files: vec!["/repo/src/lib.rs".into()],
                session_files: vec!["/repo/src/lib.rs".into()],
            },
        ];

        let out = flatten_transcript(&entries);
        let call = out
            .iter()
            .find_map(|item| match item {
                OutputItem::ToolDispatch { calls } => calls.first(),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            call.ended_at.unwrap().duration_since(call.started_at),
            std::time::Duration::from_millis(1_500)
        );
        assert!(matches!(
            out.last(),
            Some(OutputItem::ActivitySummary { turn, session })
                if turn.file_count() == 1
                    && turn.insertions == 7
                    && turn.deletions == 3
                    && session.file_count() == 1
        ));
        let app = crate::app::AppState::new("session".into(), None).with_initial_items(out);
        assert_eq!(app.session_activity.file_count(), 1);
        assert_eq!(app.session_activity.insertions, 7);
        assert_eq!(app.session_activity.deletions, 3);
    }

    #[test]
    fn image_part_is_skipped_silently() {
        use atman_runtime::message::{ImageData, ImageSource};
        use std::path::PathBuf;
        let msgs = vec![assistant(vec![
            MessagePart::Text {
                text: "here".into(),
            },
            MessagePart::Image {
                id: None,
                source: ImageSource {
                    media_type: "image/png".into(),
                    data: ImageData::Path {
                        path: PathBuf::from("/tmp/x.png"),
                    },
                    detail: atman_runtime::provider::ImageDetail::Auto,
                },
            },
        ])];
        let out = flatten_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OutputItem::AssistantMd { .. }));
    }

    #[test]
    fn internal_context_record_is_hidden_from_transcript() {
        let message = Message::context_record(
            TurnId::now(),
            atman_runtime::ContextRecord::new(
                "session.goal",
                1,
                atman_runtime::ContextRecordAuthority::User,
                atman_runtime::ContextRecordRetention::Latest,
                atman_runtime::ContextRecordBody::text("private model context"),
            ),
        );

        assert!(flatten_messages(&[message]).is_empty());
    }

    #[test]
    fn flatten_transcript_dedup_same_handle_terminal_with_final_state() {
        let handle = "term_s_0";
        let mk_tool_pair = |id: &str, text: &str| -> Vec<TranscriptEntry> {
            vec![
                TranscriptEntry::Message {
                    message: Message {
                        role: MessageRole::Assistant,
                        parts: vec![MessagePart::ToolUse {
                            id: id.into(),
                            name: "term.capture".into(),
                            input: serde_json::json!({}),
                            intent: None,
                        }],
                        turn_id: TurnId::now(),
                        origin: atman_runtime::message::MessageOrigin::User,
                    },
                    flow_run_id: None,
                },
                TranscriptEntry::Message {
                    message: Message {
                        role: MessageRole::Tool,
                        parts: vec![MessagePart::ToolResult {
                            tool_use_id: id.into(),
                            content: format!(
                                r#"{{"handle":"{handle}","state":{{"kind":"running"}},"rows":2,"cols":3,"text":"{text}"}}"#
                            ),
                            is_error: false,
                        }],
                        turn_id: TurnId::now(),
                        origin: atman_runtime::message::MessageOrigin::User,
                    },
                    flow_run_id: None,
                },
            ]
        };
        let mut entries = Vec::new();
        entries.extend(mk_tool_pair("c1", "aaa"));
        entries.extend(mk_tool_pair("c2", "bbb"));
        entries.extend(mk_tool_pair("c3", "ccc"));
        entries.push(TranscriptEntry::TerminalFinalState {
            handle: handle.into(),
            screen: atman_runtime::tools::term::TerminalScreen {
                rows: 2,
                cols: 3,
                cells: vec![atman_runtime::tools::term::TerminalCell::default(); 6],
                cursor: None,
                alt_screen: false,
            },
        });
        let out = flatten_transcript(&entries);
        let terminals = out
            .iter()
            .filter_map(|item| match item {
                OutputItem::ToolDispatch { calls } => Some(
                    calls
                        .iter()
                        .filter(|call| {
                            matches!(call.detail.as_deref(), Some(OutputItem::Terminal { .. }))
                        })
                        .count(),
                ),
                _ => None,
            })
            .sum::<usize>();
        assert_eq!(
            terminals, 3,
            "each capture remains attached to its invocation"
        );
    }

    #[test]
    fn flatten_transcript_builds_workflow_panel_from_events() {
        use atman_runtime::nodegraph::FlowGraph as StaticFlowGraph;
        let entries = vec![
            TranscriptEntry::FlowGraph {
                run_id: "r1".into(),
                flow_name: "look_into".into(),
                graph: StaticFlowGraph {
                    flow_name: "look_into".into(),
                    root: Vec::new(),
                },
                ts: None,
            },
            TranscriptEntry::FlowNodeEnd {
                run_id: "r1".into(),
                node_id: "stmt_0".into(),
                status: FlowNodeStatus::Ok,
                output_preview: None,
                ts: None,
            },
            TranscriptEntry::FlowDone {
                run_id: "r1".into(),
                ok: true,
                cancelled: false,
                ts: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let panel = out
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel {
                    graph, ended_at, ..
                } => Some((graph, *ended_at)),
                _ => None,
            })
            .expect("workflow panel");
        assert_eq!(panel.0.root.len(), 1);
        assert_eq!(panel.0.root[0].label, "look_into");
        assert!(panel.1.is_some(), "FlowDone should close panel");
    }

    #[test]
    fn flatten_transcript_session_push_user_msg_does_not_break_workflow() {
        // Reproduces: session.push(message.user(...)) inside agent flow
        // produces user_msg with flow_run_id=None between flow events.
        // This should NOT close the workflow panel or create a new empty one.
        use atman_runtime::nodegraph::{FlowGraph as StaticFlowGraph, NodeKind};
        let run_id = "agent-run-001";
        let entries = vec![
            // Genuine user message (start of turn)
            TranscriptEntry::Message {
                message: Message::user_text(
                    atman_runtime::event::TurnId::now(),
                    "hello".to_string(),
                ),
                flow_run_id: None,
            },
            // Agent flow starts
            TranscriptEntry::FlowStart {
                run_id: run_id.into(),
                flow_name: "agent".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
                ts: None,
            },
            TranscriptEntry::FlowGraph {
                run_id: run_id.into(),
                flow_name: "agent".into(),
                graph: StaticFlowGraph {
                    flow_name: "agent".into(),
                    root: Vec::new(),
                },
                ts: None,
            },
            // Flow node 0: session.push
            TranscriptEntry::FlowNodeStart {
                run_id: run_id.into(),
                node_id: "0".into(),
                kind: NodeKind::ToolCall {
                    path: "session.push".into(),
                },
                label: "session.push".into(),
                parent_node_id: None,
                ts: None,
            },
            // THIS is the problematic event: user_msg from session.push(message.user(...))
            TranscriptEntry::Message {
                message: Message::user_text(
                    atman_runtime::event::TurnId::now(),
                    "hello".to_string(),
                ),
                flow_run_id: None, // None! — same as genuine user message
            },
            // Flow node 0 ends
            TranscriptEntry::FlowNodeEnd {
                run_id: run_id.into(),
                node_id: "0".into(),
                status: FlowNodeStatus::Ok,
                output_preview: None,
                ts: None,
            },
            // Flow node 1: loop
            TranscriptEntry::FlowNodeStart {
                run_id: run_id.into(),
                node_id: "1".into(),
                kind: NodeKind::Return,
                label: "loop".into(),
                parent_node_id: None,
                ts: None,
            },
            TranscriptEntry::FlowNodeEnd {
                run_id: run_id.into(),
                node_id: "1".into(),
                status: FlowNodeStatus::Ok,
                output_preview: None,
                ts: None,
            },
            // Flow ends
            TranscriptEntry::FlowDone {
                run_id: run_id.into(),
                ok: true,
                cancelled: false,
                ts: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let panels: Vec<_> = out
            .iter()
            .filter_map(|it| match it {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph.clone()),
                _ => None,
            })
            .collect();
        // Should have exactly 1 panel (not 2 or 3)
        assert_eq!(
            panels.len(),
            1,
            "should have 1 workflow panel, got {}",
            panels.len()
        );
        // Root should have 1 node (the agent flow)
        assert_eq!(panels[0].root.len(), 1, "root should have 1 flow node");
        // The flow node should have 2 children (session.push + loop)
        let children = &panels[0].root[0].children;
        assert_eq!(
            children.len(),
            2,
            "flow node should have 2 children, got {}",
            children.len()
        );
        assert_eq!(children[0].label, "session.push");
        assert_eq!(children[1].label, "loop");
    }

    #[test]
    fn flatten_transcript_closes_orphan_panels_without_flow_done() {
        use atman_runtime::nodegraph::FlowGraph as StaticFlowGraph;
        let entries = vec![TranscriptEntry::FlowGraph {
            run_id: "r1".into(),
            flow_name: "agent".into(),
            graph: StaticFlowGraph {
                flow_name: "agent".into(),
                root: Vec::new(),
            },
            ts: None,
        }];
        let out = flatten_transcript(&entries);
        let panel = out
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel { ended_at, .. } => Some(*ended_at),
                _ => None,
            })
            .expect("workflow panel");
        assert!(
            panel.is_some(),
            "orphan panel without FlowDone must be closed on restore"
        );
    }

    #[test]
    fn flatten_transcript_marks_orphan_sub_agent_as_interrupted() {
        let root_run = "root-001".to_string();
        let sub_run = "sub-001".to_string();

        let entries = vec![
            TranscriptEntry::FlowStart {
                run_id: root_run.clone(),
                flow_name: "agent".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
                ts: None,
            },
            TranscriptEntry::FlowStart {
                run_id: sub_run.clone(),
                flow_name: "subagent".into(),
                parent_run_id: Some(root_run.clone()),
                parent_node_id: Some("1".into()),
                spawned: true,
                ts: None,
            },
            TranscriptEntry::Message {
                message: Message::user_text(TurnId::now(), "do research"),
                flow_run_id: Some(sub_run.clone()),
            },
            // No FlowDone for sub_run — simulates crash/restart mid-subflow
        ];

        let out = flatten_transcript(&entries);
        let sub = out
            .iter()
            .find_map(|it| match it {
                OutputItem::SubAgentActivity { done, status, .. } => Some((*done, status.clone())),
                _ => None,
            })
            .expect("SubAgentActivity item");
        assert!(sub.0, "orphan sub-agent must be marked done on restore");
        assert_eq!(
            sub.1, "interrupted",
            "orphan sub-agent status must be 'interrupted'"
        );
    }

    #[test]
    fn flatten_transcript_restores_bash_panel_from_tool_result() {
        let tool_use_id = "call_00_bash1";
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "bash.spawn".into(),
                        input: serde_json::json!({"cmd": "cargo test --workspace"}),
                        intent: atman_runtime::message::ToolCallIntent::new("运行项目测试"),
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content: r#"{"handle":"bg_1","output":"hello\nworld","status":"exited","exit_code":0}"#.into(),
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let bash = tool_detail(&out, tool_use_id).and_then(|it| match it {
            OutputItem::Bash {
                title,
                command,
                output,
                ..
            } => Some((title.clone(), command.clone(), output.clone())),
            _ => None,
        });
        assert_eq!(
            bash,
            Some((
                Some("运行项目测试".into()),
                Some("cargo test --workspace".into()),
                "hello\nworld".into()
            ))
        );
    }

    #[test]
    fn dedup_preserves_spawn_command_on_later_output_item() {
        let mut items = vec![
            OutputItem::Bash {
                handle: "bg_1".into(),
                title: Some("运行测试".into()),
                command: Some("cargo test --workspace".into()),
                output: "started".into(),
                done: true,
                expanded: false,
            },
            OutputItem::Bash {
                handle: "bg_1".into(),
                title: None,
                command: None,
                output: "completed".into(),
                done: true,
                expanded: false,
            },
        ];
        dedup_by_handle(&mut items);
        assert_eq!(items.len(), 1);
        let OutputItem::Bash { title, command, .. } = &items[0] else {
            panic!("expected bash item");
        };
        assert_eq!(title.as_deref(), Some("运行测试"));
        assert_eq!(command.as_deref(), Some("cargo test --workspace"));
    }

    #[test]
    fn dedup_removes_later_empty_item_and_keeps_its_metadata() {
        let mut items = vec![
            OutputItem::Bash {
                handle: "bg_1".into(),
                title: None,
                command: None,
                output: "completed".into(),
                done: true,
                expanded: false,
            },
            OutputItem::Bash {
                handle: "bg_1".into(),
                title: Some("运行测试".into()),
                command: Some("cargo test --workspace".into()),
                output: String::new(),
                done: true,
                expanded: false,
            },
        ];

        dedup_by_handle(&mut items);

        assert_eq!(items.len(), 1);
        let OutputItem::Bash {
            title,
            command,
            output,
            ..
        } = &items[0]
        else {
            panic!("expected bash item");
        };
        assert_eq!(title.as_deref(), Some("运行测试"));
        assert_eq!(command.as_deref(), Some("cargo test --workspace"));
        assert_eq!(output, "completed");
    }

    #[test]
    fn flatten_transcript_restores_bash_from_log_path() {
        let dir = std::env::temp_dir().join(format!("atman_test_bash_log_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("bg_test1.log");
        std::fs::write(&log_path, "[out] line1\n[out] line2\n").unwrap();

        let tool_use_id = "call_00_bash2";
        let content = format!(
            r#"{{"handle":"bg_test1","status":"running","pid":123,"log_path":"{}"}}"#,
            log_path.display()
        );
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "bash.spawn".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content,
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let bash = tool_detail(&out, tool_use_id).and_then(|it| match it {
            OutputItem::Bash { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(bash.as_deref(), Some("line1\nline2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flatten_transcript_bash_log_missing_falls_back_to_json() {
        let tool_use_id = "call_00_bash3";
        let fake_path = "/tmp/atman_nonexistent_bg_test.log";
        let content = format!(
            r#"{{"handle":"bg_test2","status":"running","pid":123,"log_path":"{}"}}"#,
            fake_path
        );
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "bash.spawn".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content,
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let bash = tool_detail(&out, tool_use_id).and_then(|it| match it {
            OutputItem::Bash { output, .. } => Some(output.clone()),
            _ => None,
        });
        let bash = bash.expect("bash item should exist");
        assert!(
            bash.contains("[log file removed:"),
            "should show log removed notice: {bash}"
        );
        assert!(
            bash.contains("\"handle\":\"bg_test2\""),
            "should include original JSON: {bash}"
        );
    }

    #[test]
    fn flatten_transcript_restores_diff_preview_from_fs_edit() {
        let tool_use_id = "call_00_edit1";
        let diff = "--- a.rs\n+++ b.rs\n@@ -1,1 +1,2 @@\n-old\n+new\n";
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "fs.edit".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
            TranscriptEntry::DiffPreview {
                tool_use_id: Some(tool_use_id.into()),
                title: "a.rs".into(),
                old_content: None,
                new_content: None,
                unified_diff: Some(diff.into()),
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content: format!(r#"{{"path":"a.rs","diff":{}}}"#, serde_json::json!(diff)),
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let diff_item = tool_detail(&out, tool_use_id).and_then(|it| match it {
            OutputItem::DiffPreview { unified_diff, .. } => unified_diff.clone(),
            _ => None,
        });
        assert_eq!(diff_item.as_deref(), Some(diff));
        assert!(
            out.iter()
                .all(|item| !matches!(item, OutputItem::DiffPreview { .. })),
            "correlated previews must stay inside their tool row"
        );
    }

    #[test]
    fn flatten_transcript_scopes_persisted_tool_result_from_tool_node() {
        use atman_runtime::nodegraph::NodeKind;
        use atman_runtime::workflow::NodeStatus;

        let run_id = "run-persisted-tool".to_string();
        let entries = vec![
            TranscriptEntry::FlowStart {
                run_id: run_id.clone(),
                flow_name: "agent_loop".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
                ts: None,
            },
            TranscriptEntry::FlowNodeStart {
                run_id: run_id.clone(),
                node_id: "dispatch_all".into(),
                kind: NodeKind::ToolCall {
                    path: "dispatch_all".into(),
                },
                label: "dispatch_all".into(),
                parent_node_id: None,
                ts: None,
            },
            TranscriptEntry::ToolNode {
                run_id: run_id.clone(),
                parent_node_id: "dispatch_all".into(),
                tool_use_id: "tu_persisted".into(),
                tool_name: "fs.read".into(),
                args_preview: String::new(),
                call_intent: None,
                ts: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: "tu_persisted".into(),
                        content: "contents".into(),
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                    origin: atman_runtime::message::MessageOrigin::User,
                },
                flow_run_id: None,
            },
        ];

        let out = flatten_transcript(&entries);
        let graph = out
            .iter()
            .find_map(|item| match item {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .expect("workflow panel");
        let dispatch = graph
            .find_node(&format!("{run_id}::dispatch_all"))
            .expect("dispatch node");
        assert_eq!(dispatch.children.len(), 1);
        assert_eq!(dispatch.children[0].status, NodeStatus::Ok);
        assert_eq!(
            dispatch.children[0].output_preview.as_deref(),
            Some("contents")
        );
    }

    #[test]
    fn flatten_transcript_skips_unknown_tool_results() {
        let tool_use_id = "call_00_unknown";
        let entries = vec![TranscriptEntry::Message {
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: tool_use_id.into(),
                    content: r#"{"result":"ok"}"#.into(),
                    is_error: false,
                }],
                turn_id: TurnId::now(),
                origin: atman_runtime::message::MessageOrigin::User,
            },
            flow_run_id: None,
        }];
        let out = flatten_transcript(&entries);
        assert!(
            out.is_empty(),
            "unknown tool without error should not produce an item"
        );
    }

    #[test]
    fn replay_routes_spawned_messages_to_sub_agent_activity() {
        let root_run = "root-run-001".to_string();
        let loop_run = "loop-run-001".to_string();
        let sub_run = "sub-run-001".to_string();
        let research_flow_run_id = FlowRunId::now();
        let research_run = research_flow_run_id.0.to_string();

        let entries = vec![
            // Root agent starts (spawned=false)
            TranscriptEntry::FlowStart {
                run_id: root_run.clone(),
                flow_name: "agent".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
                ts: None,
            },
            // Root's agent_loop subflow (spawned=false, parent=root)
            TranscriptEntry::FlowStart {
                run_id: loop_run.clone(),
                flow_name: "agent_loop".into(),
                parent_run_id: Some(root_run.clone()),
                parent_node_id: Some("1".into()),
                spawned: false,
                ts: None,
            },
            // Root assistant message (flow_run_id = loop_run)
            TranscriptEntry::Message {
                message: Message::assistant_text(TurnId::now(), "I will spawn a sub-agent."),
                flow_run_id: Some(loop_run.clone()),
            },
            // Sub-agent spawned via flow.spawn (spawned=true, parent=loop_run)
            TranscriptEntry::FlowStart {
                run_id: sub_run.clone(),
                flow_name: "subagent".into(),
                parent_run_id: Some(loop_run.clone()),
                parent_node_id: Some("7".into()),
                spawned: true,
                ts: None,
            },
            // Sub-agent's research_loop subflow (spawned=false, parent=sub_run)
            TranscriptEntry::FlowStart {
                run_id: research_run.clone(),
                flow_name: "research_loop".into(),
                parent_run_id: Some(sub_run.clone()),
                parent_node_id: Some("7".into()),
                spawned: false,
                ts: None,
            },
            TranscriptEntry::LlmCall {
                model: "primary-model".into(),
                provider: "provider".into(),
                context_call_purpose: Default::default(),
                context_call_scope: Default::default(),
                usage: Default::default(),
                wallclock_ms: 1,
                ttft_ms: Some(1),
                tokens_per_second: Some(1.0),
                run_id: Some(research_flow_run_id.clone()),
                node_id: None,
                ts: None,
            },
            // Sub-agent user message (flow_run_id = research_run)
            TranscriptEntry::Message {
                message: Message::user_text(TurnId::now(), "read Cargo.toml"),
                flow_run_id: Some(research_run.clone()),
            },
            // Sub-agent flow node (tool call)
            TranscriptEntry::FlowNodeStart {
                run_id: research_run.clone(),
                node_id: "stmt_0".into(),
                kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                    path: "fs.read".into(),
                },
                label: "fs.read".into(),
                parent_node_id: None,
                ts: None,
            },
            TranscriptEntry::ToolNode {
                run_id: research_run.clone(),
                parent_node_id: "stmt_0".into(),
                tool_use_id: "spawned-tool".into(),
                tool_name: "fs.read".into(),
                args_preview: "Cargo.toml".into(),
                call_intent: None,
                ts: None,
            },
            approved_permission(research_flow_run_id, "spawned-tool"),
            // Sub-agent assistant message (flow_run_id = research_run)
            TranscriptEntry::Message {
                message: Message::assistant_text(TurnId::now(), "Here are the findings..."),
                flow_run_id: Some(research_run.clone()),
            },
            TranscriptEntry::FlowNodeEnd {
                run_id: research_run.clone(),
                node_id: "stmt_0".into(),
                status: atman_runtime::event::FlowNodeStatus::Ok,
                output_preview: Some("ok".into()),
                ts: None,
            },
            // Sub-agent done
            TranscriptEntry::FlowDone {
                run_id: sub_run.clone(),
                ok: true,
                cancelled: false,
                ts: None,
            },
            TranscriptEntry::FlowDone {
                run_id: research_run.clone(),
                ok: true,
                cancelled: false,
                ts: None,
            },
        ];

        let out = flatten_transcript(&entries);

        // Root agent message should be in the main document flow
        let has_root_text = out.iter().any(|it| match it {
            OutputItem::AssistantMd { md, .. } => md.contains("spawn a sub-agent"),
            _ => false,
        });
        assert!(has_root_text, "root agent message should be in main flow");

        // Sub-agent messages should NOT be in the main document flow
        let has_sub_text = out.iter().any(|it| match it {
            OutputItem::AssistantMd { md, .. } => md.contains("findings"),
            OutputItem::UserTurn { text } => text.contains("Cargo.toml"),
            _ => false,
        });
        assert!(
            !has_sub_text,
            "sub-agent messages should NOT leak into main flow"
        );

        // SubAgentActivity item should exist
        let sub_item = out.iter().find_map(|it| match it {
            OutputItem::SubAgentActivity {
                child_run_id,
                messages,
                goal,
                model,
                status,
                workflow_graph,
                ..
            } if child_run_id == &sub_run => Some((
                messages.len(),
                goal.clone(),
                model.clone(),
                status.clone(),
                workflow_graph.root.len(),
                workflow_graph
                    .find_node(&format!("tool:{research_run}:spawned-tool"))
                    .and_then(|node| node.approval.clone()),
            )),
            _ => None,
        });
        assert!(sub_item.is_some(), "SubAgentActivity should be created");
        let (msg_count, goal, model, status, graph_nodes, approval) = sub_item.unwrap();
        assert_eq!(msg_count, 2, "should have 2 messages (user + assistant)");
        assert_eq!(goal, "read Cargo.toml", "goal from first user message");
        assert_eq!(model, "primary-model");
        assert_eq!(status, "ok", "status from FlowDone");
        assert!(
            graph_nodes > 0,
            "workflow_graph should have nodes after rebuild, got {graph_nodes}"
        );
        assert_eq!(
            approval,
            Some(atman_runtime::workflow::ApprovalState::Approved)
        );
    }

    #[test]
    fn replay_does_not_route_root_subflow_to_sub_agent() {
        // Regression: root agent's agent_loop subflow has parent_run_id set,
        // but spawned=false. Its messages must stay in the main flow.
        let root_run = "root-002".to_string();
        let loop_run = "loop-002".to_string();

        let entries = vec![
            TranscriptEntry::FlowStart {
                run_id: root_run.clone(),
                flow_name: "agent".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
                ts: None,
            },
            TranscriptEntry::FlowStart {
                run_id: loop_run.clone(),
                flow_name: "agent_loop".into(),
                parent_run_id: Some(root_run.clone()),
                parent_node_id: Some("1".into()),
                spawned: false,
                ts: None,
            },
            TranscriptEntry::Message {
                message: Message::assistant_text(TurnId::now(), "working on it"),
                flow_run_id: Some(loop_run.clone()),
            },
        ];

        let out = flatten_transcript(&entries);

        // Should be in main flow, not SubAgentActivity
        let has_text = out.iter().any(|it| match it {
            OutputItem::AssistantMd { md, .. } => md.contains("working on it"),
            _ => false,
        });
        assert!(has_text, "root subflow message should be in main flow");

        let has_sub = out
            .iter()
            .any(|it| matches!(it, OutputItem::SubAgentActivity { .. }));
        assert!(
            !has_sub,
            "should not create SubAgentActivity for root subflow"
        );
    }

    #[test]
    fn spawned_flow_events_do_not_leak_to_main_workflow_panel() {
        let root_run = "root-003".to_string();
        let loop_run = "loop-003".to_string();
        let sub_run = "sub-003".to_string();
        let research_run = "research-003".to_string();

        let entries = vec![
            TranscriptEntry::FlowStart {
                run_id: root_run.clone(),
                flow_name: "agent".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
                ts: None,
            },
            TranscriptEntry::FlowStart {
                run_id: loop_run.clone(),
                flow_name: "agent_loop".into(),
                parent_run_id: Some(root_run.clone()),
                parent_node_id: Some("1".into()),
                spawned: false,
                ts: None,
            },
            // Root's tool node — should appear in main workflow panel
            TranscriptEntry::FlowNodeStart {
                run_id: loop_run.clone(),
                node_id: "stmt_0".into(),
                kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                    path: "fs.read".into(),
                },
                label: "fs.read".into(),
                parent_node_id: None,
                ts: None,
            },
            TranscriptEntry::FlowNodeEnd {
                run_id: loop_run.clone(),
                node_id: "stmt_0".into(),
                status: atman_runtime::event::FlowNodeStatus::Ok,
                output_preview: Some("ok".into()),
                ts: None,
            },
            // Sub-agent spawned
            TranscriptEntry::FlowStart {
                run_id: sub_run.clone(),
                flow_name: "subagent".into(),
                parent_run_id: Some(loop_run.clone()),
                parent_node_id: Some("7".into()),
                spawned: true,
                ts: None,
            },
            TranscriptEntry::FlowStart {
                run_id: research_run.clone(),
                flow_name: "research_loop".into(),
                parent_run_id: Some(sub_run.clone()),
                parent_node_id: Some("7".into()),
                spawned: false,
                ts: None,
            },
            // Sub-agent's tool node — should NOT appear in main workflow panel
            TranscriptEntry::FlowNodeStart {
                run_id: research_run.clone(),
                node_id: "stmt_0".into(),
                kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                    path: "fs.grep".into(),
                },
                label: "fs.grep".into(),
                parent_node_id: None,
                ts: None,
            },
            TranscriptEntry::ToolNode {
                run_id: research_run.clone(),
                parent_node_id: "stmt_0".into(),
                tool_use_id: "tool_001".into(),
                tool_name: "fs.grep".into(),
                args_preview: "pattern".into(),
                call_intent: None,
                ts: None,
            },
            TranscriptEntry::FlowNodeEnd {
                run_id: research_run.clone(),
                node_id: "stmt_0".into(),
                status: atman_runtime::event::FlowNodeStatus::Ok,
                output_preview: Some("ok".into()),
                ts: None,
            },
            // Sub-agent message
            TranscriptEntry::Message {
                message: Message::user_text(TurnId::now(), "read Cargo.toml"),
                flow_run_id: Some(research_run.clone()),
            },
            TranscriptEntry::Message {
                message: Message::assistant_text(TurnId::now(), "found it"),
                flow_run_id: Some(research_run.clone()),
            },
            TranscriptEntry::FlowDone {
                run_id: research_run.clone(),
                ok: true,
                cancelled: false,
                ts: None,
            },
            TranscriptEntry::FlowDone {
                run_id: sub_run.clone(),
                ok: true,
                cancelled: false,
                ts: None,
            },
            TranscriptEntry::FlowDone {
                run_id: loop_run.clone(),
                ok: true,
                cancelled: false,
                ts: None,
            },
            TranscriptEntry::FlowDone {
                run_id: root_run.clone(),
                ok: true,
                cancelled: false,
                ts: None,
            },
        ];

        let out = flatten_transcript(&entries);

        // Main workflow panel should exist
        let wf_panel = out.iter().find_map(|it| match it {
            OutputItem::WorkflowPanel { graph, .. } => Some(graph.clone()),
            _ => None,
        });
        assert!(wf_panel.is_some(), "main workflow panel should exist");
        let graph = wf_panel.unwrap();

        // Main panel should have the ROOT's tool node (fs.read)
        let root_node_count = count_workflow_nodes(&graph.root);
        assert!(
            root_node_count > 0,
            "main panel should have root's nodes, got {root_node_count}"
        );

        // SubAgentActivity should exist with its own workflow graph
        let sub_graph = out.iter().find_map(|it| match it {
            OutputItem::SubAgentActivity {
                child_run_id,
                workflow_graph,
                ..
            } if child_run_id == &sub_run => Some(workflow_graph.clone()),
            _ => None,
        });
        assert!(sub_graph.is_some(), "SubAgentActivity should exist");
        let sub_graph = sub_graph.unwrap();
        let sub_node_count = count_workflow_nodes(&sub_graph.root);
        assert!(
            sub_node_count > 0,
            "SubAgentActivity graph should have nodes, got {sub_node_count}"
        );

        // CRITICAL: main panel should NOT have the sub-agent's tool node (fs.grep)
        // Check by looking at the main panel's node labels
        let main_labels = collect_node_labels(&graph.root);
        assert!(
            main_labels.iter().any(|l| l.contains("fs.read")),
            "main panel should have fs.read node"
        );
        assert!(
            !main_labels.iter().any(|l| l.contains("fs.grep")),
            "main panel should NOT have fs.grep node (sub-agent's tool), found: {main_labels:?}"
        );
    }

    fn count_workflow_nodes(nodes: &[atman_runtime::workflow::WorkflowNode]) -> usize {
        let mut count = nodes.len();
        for node in nodes {
            count += count_workflow_nodes(&node.children);
        }
        count
    }

    fn collect_node_labels(nodes: &[atman_runtime::workflow::WorkflowNode]) -> Vec<String> {
        let mut labels = Vec::new();
        for node in nodes {
            labels.push(node.label.clone());
            labels.extend(collect_node_labels(&node.children));
        }
        labels
    }

    fn collect_leaked_flow_nodes(
        nodes: &[atman_runtime::workflow::WorkflowNode],
        spawned_run_ids: &HashSet<String>,
        leaked: &mut Vec<String>,
    ) {
        for node in nodes {
            if let atman_runtime::workflow::WorkflowNodeKind::Flow { run_id, .. } = &node.kind {
                if spawned_run_ids.contains(run_id) {
                    leaked.push(run_id.clone());
                }
            }
            collect_leaked_flow_nodes(&node.children, spawned_run_ids, leaked);
        }
    }

    #[test]
    fn verify_real_session_63ae6ed0() {
        let path = std::path::Path::new(
            "/Users/w-mai/Library/Application Support/atman/sessions/63ae6ed0-77da-486e-9246-38c53875b32a/events.jsonl",
        );
        if !path.exists() {
            eprintln!("skipping: session file not found");
            return;
        }
        let entries =
            atman_runtime::projection::message_window::replay_transcript_from(path).unwrap();
        let out = flatten_transcript(&entries);

        let sub_items: Vec<_> = out
            .iter()
            .filter_map(|it| match it {
                OutputItem::SubAgentActivity {
                    handle,
                    workflow_graph,
                    status,
                    ..
                } => Some((handle.clone(), workflow_graph.root.len(), status.clone())),
                _ => None,
            })
            .collect();

        eprintln!("=== SubAgentActivity items: {}", sub_items.len());
        for (handle, nodes, status) in &sub_items {
            eprintln!("  handle={} nodes={} status={}", handle, nodes, status);
        }

        // Check main workflow panel for sub-agent nodes
        let main_panels: Vec<_> = out
            .iter()
            .filter_map(|it| match it {
                OutputItem::WorkflowPanel { graph, .. } => Some(count_workflow_nodes(&graph.root)),
                _ => None,
            })
            .collect();
        eprintln!(
            "=== Main workflow panels: {}, node counts: {:?}",
            main_panels.len(),
            main_panels
        );

        assert!(!sub_items.is_empty(), "should have SubAgentActivity items");
        for (handle, nodes, status) in &sub_items {
            assert!(
                *nodes > 0,
                "SubAgentActivity {} should have nodes, got {}",
                handle,
                nodes
            );
            assert_eq!(status, "ok", "SubAgentActivity {} should be done", handle);
        }

        // CRITICAL: verify no main workflow panel contains a Flow node
        // whose run_id is in the spawned set (would mean spawned events leaked)
        let spawned_run_ids: HashSet<String> =
            sub_items.iter().map(|(h, _, _)| h.clone()).collect();
        let mut leaked: Vec<String> = Vec::new();
        for item in &out {
            if let OutputItem::WorkflowPanel { graph, .. } = item {
                collect_leaked_flow_nodes(&graph.root, &spawned_run_ids, &mut leaked);
            }
        }
        assert!(
            leaked.is_empty(),
            "spawned flow nodes leaked into main workflow panels: {:?}",
            leaked
        );
    }

    #[test]
    fn verify_a5e33999() {
        let path = std::path::Path::new(
            "/Users/w-mai/Library/Application Support/atman/sessions/a5e33999-0e74-40f8-82f2-5a489916bbeb/events.jsonl",
        );
        if !path.exists() {
            eprintln!("skipping: session file not found");
            return;
        }
        let entries =
            atman_runtime::projection::message_window::replay_transcript_from(path).unwrap();
        let out = flatten_transcript(&entries);

        let wf_count = out
            .iter()
            .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
            .count();
        let sub_count = out
            .iter()
            .filter(|it| matches!(it, OutputItem::SubAgentActivity { .. }))
            .count();
        eprintln!("WorkflowPanel count: {}", wf_count);
        eprintln!("SubAgentActivity count: {}", sub_count);

        for (i, item) in out.iter().enumerate() {
            if let OutputItem::WorkflowPanel { graph, .. } = item {
                let node_count = count_workflow_nodes(&graph.root);
                let labels = collect_node_labels(&graph.root);
                eprintln!(
                    "  WF[{}] nodes={} labels={:?}",
                    i,
                    node_count,
                    &labels[..labels.len().min(5)]
                );
            }
        }

        for (i, item) in out.iter().enumerate() {
            if let OutputItem::SubAgentActivity {
                handle,
                workflow_graph,
                status,
                ..
            } = item
            {
                let node_count = count_workflow_nodes(&workflow_graph.root);
                eprintln!(
                    "  SUB[{}] handle={} nodes={} status={}",
                    i, handle, node_count, status
                );
            }
        }
    }
}
