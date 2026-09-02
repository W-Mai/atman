use std::collections::HashMap;
use std::path::Path;

use crate::event;
#[cfg(test)]
use crate::event_log::reader::parse_json_lines;
use crate::event_log::reader::read_event_envelopes;
use crate::message::{Message, MessagePart};
use crate::nodegraph;
use crate::provider;
use crate::session::SessionOpenError;

#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    Message {
        message: Message,
        flow_run_id: Option<String>,
    },
    CompactionSummary {
        range_start: usize,
        range_end: usize,
        compacted_count: usize,
        before_tokens: u64,
        after_tokens: u64,
        summary: String,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    DiffPreview {
        tool_use_id: Option<String>,
        title: String,
        old_content: Option<String>,
        new_content: Option<String>,
        unified_diff: Option<String>,
    },
    FileEditApplied {
        turn_id: Option<String>,
        flow_run_id: Option<String>,
        tool_use_id: Option<String>,
        tool_name: String,
        path: String,
        metrics: crate::activity::EditMetrics,
    },
    FlowGraph {
        run_id: String,
        flow_name: String,
        graph: nodegraph::FlowGraph,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowStart {
        run_id: String,
        flow_name: String,
        parent_run_id: Option<String>,
        parent_node_id: Option<String>,
        spawned: bool,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowNodeStart {
        run_id: String,
        node_id: String,
        kind: nodegraph::NodeKind,
        label: String,
        parent_node_id: Option<String>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowNodeEnd {
        run_id: String,
        node_id: String,
        status: event::FlowNodeStatus,
        output_preview: Option<String>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    ToolNode {
        run_id: String,
        parent_node_id: String,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        call_intent: Option<crate::message::ToolCallIntent>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowDone {
        run_id: String,
        ok: bool,
        cancelled: bool,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    LlmCall {
        model: String,
        provider: String,
        context_call_purpose: crate::context_plan::ContextCallPurpose,
        context_call_scope: crate::context_plan::ContextCallScope,
        usage: provider::TokenUsage,
        wallclock_ms: u64,
        ttft_ms: Option<u64>,
        tokens_per_second: Option<f64>,
        run_id: Option<event::FlowRunId>,
        node_id: Option<String>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    PermissionRequest {
        identity: crate::workflow::WorkflowPermissionIdentity,
        payload: Box<crate::permission_audit::PermissionRequestAudit>,
        state: crate::workflow::WorkflowPermissionState,
    },
    PermissionGroup {
        payload: crate::permission_audit::PermissionGroupAudit,
        resolved: bool,
    },
    TerminalFinalState {
        handle: String,
        screen: crate::tools::term::TerminalScreen,
    },
    MermaidDiagram {
        source: String,
    },
}

pub fn replay_messages_from(path: &Path) -> Result<Vec<Message>, SessionOpenError> {
    Ok(replay_messages_with_seq(path)?
        .into_iter()
        .map(|(_, msg)| msg)
        .collect())
}

pub fn replay_messages_with_seq(path: &Path) -> Result<Vec<(u64, Message)>, SessionOpenError> {
    let envelopes = read_event_envelopes(path)?;
    Ok(envelopes.as_slice().to_messages_with_seq())
}

pub fn replay_all_messages_with_seq(path: &Path) -> Result<Vec<(u64, Message)>, SessionOpenError> {
    let envelopes = read_event_envelopes(path)?;
    let spawned_flow_ids = spawned_flow_ids(&envelopes);
    let mut messages = Vec::new();
    let mut positions = HashMap::new();
    for env in &envelopes {
        match &env.event {
            crate::event::Event::UserMsg {
                message,
                flow_run_id,
                ..
            }
            | crate::event::Event::AssistantMsg {
                message,
                flow_run_id,
                ..
            }
            | crate::event::Event::ToolResultMsg {
                message,
                flow_run_id,
                ..
            } if message_belongs_to_root(flow_run_id.as_ref(), &spawned_flow_ids) => {
                positions.insert(env.seq, messages.len());
                messages.push((env.seq, message.clone()));
            }
            crate::event::Event::SystemMsg {
                message,
                flow_run_id,
                ..
            } if message_belongs_to_root(flow_run_id.as_ref(), &spawned_flow_ids) => {
                positions.insert(env.seq, messages.len());
                messages.push((env.seq, message.clone()));
            }
            crate::event::Event::AttachmentDegraded {
                message_seq,
                part_index,
                file_basename,
                reason,
                ..
            } => {
                apply_attachment_degradation(
                    &mut messages,
                    &positions,
                    *message_seq,
                    *part_index,
                    file_basename,
                    reason,
                );
            }
            _ => {}
        }
    }
    Ok(messages)
}

#[derive(Debug, Clone)]
pub struct AttachmentPatch {
    part_index: usize,
    file_basename: String,
    reason: String,
}

#[cfg(test)]
pub fn parse_ts(v: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.get("ts")?
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

#[cfg(test)]
pub fn parse_context_compact_event(v: &serde_json::Value) -> Option<CompactReplayEvent> {
    if v["type"].as_str() != Some("context_compact") {
        return None;
    }
    Some(CompactReplayEvent {
        range_start: v["compacted_range_start"].as_u64().unwrap_or(0) as usize,
        range_end: v["compacted_range_end"].as_u64().unwrap_or(0) as usize,
        replacement_msg_seq: v["replacement_msg_seq"].as_u64(),
    })
}

#[cfg(test)]
fn raw_event_belongs_to_root(
    value: &serde_json::Value,
    spawned_flow_ids: &std::collections::HashSet<crate::event::FlowRunId>,
) -> bool {
    value["flow_run_id"]
        .as_str()
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
        .map(crate::event::FlowRunId)
        .is_none_or(|run_id| !spawned_flow_ids.contains(&run_id))
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct CompactReplayEvent {
    range_start: usize,
    range_end: usize,
    replacement_msg_seq: Option<u64>,
}

#[derive(Clone, Copy)]
struct TranscriptMessageSlot {
    seq: u64,
    output_index: usize,
}

fn push_transcript_message(
    out: &mut Vec<TranscriptEntry>,
    messages: &mut Vec<TranscriptMessageSlot>,
    positions: &mut HashMap<u64, usize>,
    seq: u64,
    entry: TranscriptEntry,
) {
    positions.insert(seq, messages.len());
    messages.push(TranscriptMessageSlot {
        seq,
        output_index: out.len(),
    });
    out.push(entry);
}

fn compact_transcript_messages(
    out: &mut Vec<TranscriptEntry>,
    messages: &mut Vec<TranscriptMessageSlot>,
    positions: &mut HashMap<u64, usize>,
    range_start: usize,
    range_end: usize,
    replacement_seq: u64,
) -> bool {
    if range_start > range_end || range_end >= messages.len() {
        return false;
    }
    let Some(replacement_position) = positions.get(&replacement_seq).copied() else {
        return false;
    };
    let replacement_output_index = messages[replacement_position].output_index;
    let mut replacement_entry = Some(out[replacement_output_index].clone());
    let insertion_output_index = messages[range_start].output_index;
    let removed_output_indices = messages[range_start..=range_end]
        .iter()
        .map(|slot| slot.output_index)
        .chain(std::iter::once(replacement_output_index))
        .collect::<std::collections::HashSet<_>>();

    let old_len = out.len();
    let mut old_to_new = vec![None; old_len];
    let mut replacement_new_index = None;
    let mut compacted = Vec::with_capacity(
        old_len
            .saturating_sub(removed_output_indices.len())
            .saturating_add(1),
    );
    for (old_index, entry) in out.drain(..).enumerate() {
        if old_index == insertion_output_index {
            replacement_new_index = Some(compacted.len());
            compacted.push(
                replacement_entry
                    .take()
                    .expect("replacement inserted exactly once"),
            );
        }
        if removed_output_indices.contains(&old_index) {
            continue;
        }
        old_to_new[old_index] = Some(compacted.len());
        compacted.push(entry);
    }
    let replacement_new_index = replacement_new_index.expect("message slot belongs to output");
    *out = compacted;

    let mut compacted_messages = Vec::with_capacity(
        messages
            .len()
            .saturating_sub(range_end - range_start)
            .saturating_sub(usize::from(
                replacement_position < range_start || replacement_position > range_end,
            )),
    );
    for (message_index, slot) in messages.iter().copied().enumerate() {
        if message_index == range_start {
            compacted_messages.push(TranscriptMessageSlot {
                seq: replacement_seq,
                output_index: replacement_new_index,
            });
        }
        if (range_start..=range_end).contains(&message_index)
            || message_index == replacement_position
        {
            continue;
        }
        compacted_messages.push(TranscriptMessageSlot {
            seq: slot.seq,
            output_index: old_to_new[slot.output_index]
                .expect("retained message has a retained output entry"),
        });
    }
    *messages = compacted_messages;
    positions.clear();
    positions.extend(
        messages
            .iter()
            .enumerate()
            .map(|(index, slot)| (slot.seq, index)),
    );
    true
}

#[cfg(test)]
pub fn collect_attachment_patches(
    values: &[serde_json::Value],
) -> HashMap<u64, Vec<AttachmentPatch>> {
    let mut map: HashMap<u64, Vec<AttachmentPatch>> = HashMap::new();
    for v in values {
        if v["type"].as_str() == Some("attachment_degraded") {
            let Some(msg_seq) = v["message_seq"].as_u64() else {
                continue;
            };
            let Some(part_index) = v["part_index"].as_u64() else {
                continue;
            };
            let file_basename = v["file_basename"].as_str().unwrap_or("").to_string();
            let reason = v["reason"].as_str().unwrap_or("degraded").to_string();
            map.entry(msg_seq).or_default().push(AttachmentPatch {
                part_index: part_index as usize,
                file_basename,
                reason,
            });
        }
    }
    map
}

pub fn apply_attachment_patches(msg: &mut Message, patches: &[AttachmentPatch]) {
    for p in patches {
        if let Some(part) = msg.parts.get_mut(p.part_index) {
            *part = MessagePart::Text {
                text: format!(
                    "[attachment unavailable: {} — {}]",
                    p.file_basename, p.reason
                ),
            };
        }
    }
}

fn legacy_permission_payload(
    run_id: event::FlowRunId,
    tool_use_id: String,
    tool_name: &str,
    reason: Option<&str>,
    actor_label: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> crate::permission_audit::PermissionRequestAudit {
    use crate::permission_audit::{
        PermissionAuditTarget, PermissionPolicyReference, PermissionProjectionActor,
        PermissionProvenanceSummary, PermissionRequestAudit,
    };

    PermissionRequestAudit {
        request_id: None,
        revision: 0,
        session_id: "legacy:unknown".into(),
        requesting_run_id: run_id.clone(),
        parent_run_id: None,
        root_run_id: run_id,
        tool_use_id,
        tool: if tool_name.is_empty() {
            "unknown legacy tool".into()
        } else {
            tool_name.into()
        },
        call_intent: None,
        tier: crate::tool::Tier::Zero,
        execution_boundary: Default::default(),
        provenance: PermissionProvenanceSummary {
            cwd: None,
            path: None,
            path_origin: Some("legacy_unknown".into()),
            workspace_id: None,
            workspace_root: None,
            repository_root: None,
            network: false,
            risks: Default::default(),
            targets: Vec::new(),
        },
        target: PermissionAuditTarget::User,
        group_ids: Vec::new(),
        policy: PermissionPolicyReference {
            snapshot_id: "legacy:unknown".into(),
            rule_id: "legacy approval event; policy unavailable".into(),
        },
        escalation_path: Vec::new(),
        decision_id: None,
        actor: Some(PermissionProjectionActor::UnknownLegacy {
            label: actor_label.into(),
        }),
        scope: None,
        reason: reason.map(String::from),
        at,
    }
}

pub fn replay_transcript_from(path: &Path) -> Result<Vec<TranscriptEntry>, SessionOpenError> {
    let mut entries = Vec::new();
    let mut observer = |entry| entries.push(entry);
    crate::event_log::replay::SessionReplay::from_path(path, Some(&mut observer))?;
    Ok(entries)
}

#[cfg(test)]
fn replay_transcript_from_raw(path: &Path) -> Result<Vec<TranscriptEntry>, SessionOpenError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let values = parse_json_lines(&text);
    let mut known_flow_ids = std::collections::HashSet::new();
    let mut flow_children =
        std::collections::HashMap::<crate::event::FlowRunId, Vec<crate::event::FlowRunId>>::new();
    let mut spawned_flow_ids = std::collections::HashSet::new();
    for value in &values {
        if value["type"].as_str() != Some("flow_start") {
            continue;
        }
        let Some(raw_run_id) = value["run_id"].as_str() else {
            continue;
        };
        let Ok(run_id) = uuid::Uuid::parse_str(raw_run_id) else {
            continue;
        };
        let run_id = crate::event::FlowRunId(run_id);
        let parent = value["parent_run_id"]
            .as_str()
            .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
            .map(crate::event::FlowRunId);
        known_flow_ids.insert(run_id.clone());
        if let Some(parent) = parent {
            flow_children
                .entry(parent)
                .or_default()
                .push(run_id.clone());
        }
        if value["spawned"].as_bool().unwrap_or(false) {
            spawned_flow_ids.insert(run_id);
        }
    }
    let mut queue = std::collections::VecDeque::from_iter(spawned_flow_ids.iter().cloned());
    while let Some(parent) = queue.pop_front() {
        if let Some(descendants) = flow_children.get(&parent) {
            for descendant in descendants {
                if spawned_flow_ids.insert(descendant.clone()) {
                    queue.push_back(descendant.clone());
                }
            }
        }
    }
    let patches = collect_attachment_patches(&values);
    let mut out = Vec::new();
    let mut messages = Vec::new();
    let mut message_positions = HashMap::new();
    let mut pending_permissions: std::collections::BTreeMap<
        crate::workflow::WorkflowPermissionIdentity,
        crate::permission_audit::PermissionRequestAudit,
    > = std::collections::BTreeMap::new();
    let mut legacy_pending: std::collections::HashMap<
        (String, String),
        Vec<crate::workflow::WorkflowPermissionIdentity>,
    > = std::collections::HashMap::new();
    let mut canonical_permissions = std::collections::HashSet::new();
    for v in &values {
        let ty = v["type"].as_str().unwrap_or("");
        match ty {
            "user_msg" | "assistant_msg" | "tool_result_msg" | "system_msg" => {
                if let Some(m) = v.get("message")
                    && let Ok(mut msg) = serde_json::from_value::<Message>(m.clone())
                {
                    let seq = v["seq"].as_u64().unwrap_or(0);
                    let belongs_to_root = raw_event_belongs_to_root(v, &spawned_flow_ids);
                    if let Some(ps) = patches.get(&seq) {
                        apply_attachment_patches(&mut msg, ps);
                    }
                    let flow_run_id = v["flow_run_id"].as_str().and_then(|raw| {
                        let run_id = uuid::Uuid::parse_str(raw).ok()?;
                        let run_id = crate::event::FlowRunId(run_id);
                        if known_flow_ids.contains(&run_id) && !spawned_flow_ids.contains(&run_id) {
                            None
                        } else {
                            Some(raw.to_string())
                        }
                    });
                    let entry = TranscriptEntry::Message {
                        message: msg,
                        flow_run_id,
                    };
                    if belongs_to_root {
                        push_transcript_message(
                            &mut out,
                            &mut messages,
                            &mut message_positions,
                            seq,
                            entry,
                        );
                    } else {
                        out.push(entry);
                    }
                }
            }
            "context_compact" => {
                if !raw_event_belongs_to_root(v, &spawned_flow_ids) {
                    continue;
                }
                let Some(event) = parse_context_compact_event(v) else {
                    continue;
                };
                let Some(replacement_seq) = event.replacement_msg_seq else {
                    continue;
                };
                compact_transcript_messages(
                    &mut out,
                    &mut messages,
                    &mut message_positions,
                    event.range_start,
                    event.range_end,
                    replacement_seq,
                );
            }
            "compaction_summary" => {
                if !raw_event_belongs_to_root(v, &spawned_flow_ids) {
                    continue;
                }
                out.push(TranscriptEntry::CompactionSummary {
                    range_start: v["range_start"].as_u64().unwrap_or(0) as usize,
                    range_end: v["range_end"].as_u64().unwrap_or(0) as usize,
                    compacted_count: v["compacted_count"].as_u64().unwrap_or(0) as usize,
                    before_tokens: v["before_tokens"].as_u64().unwrap_or(0),
                    after_tokens: v["after_tokens"].as_u64().unwrap_or(0),
                    summary: v["summary"].as_str().unwrap_or("").to_string(),
                    ts: parse_ts(v),
                });
            }
            "diff_preview" => {
                out.push(TranscriptEntry::DiffPreview {
                    tool_use_id: v["tool_use_id"].as_str().map(String::from),
                    title: v["title"].as_str().unwrap_or("").to_string(),
                    old_content: v["old_content"].as_str().map(String::from),
                    new_content: v["new_content"].as_str().map(String::from),
                    unified_diff: v["unified_diff"].as_str().map(String::from),
                });
            }
            "flow_graph" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let flow_name = v
                    .get("graph")
                    .and_then(|g| g["flow_name"].as_str())
                    .unwrap_or("")
                    .to_string();
                let ts = parse_ts(v);
                if let Some(g) = v.get("graph")
                    && let Ok(graph) = serde_json::from_value::<nodegraph::FlowGraph>(g.clone())
                {
                    out.push(TranscriptEntry::FlowGraph {
                        run_id,
                        flow_name,
                        graph,
                        ts,
                    });
                }
            }
            "flow_start" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let flow_name = v["flow_name"].as_str().unwrap_or("").to_string();
                let parent_run_id = v["parent_run_id"].as_str().map(String::from);
                let parent_node_id = v["parent_node_id"].as_str().map(String::from);
                let spawned = v.get("spawned").and_then(|s| s.as_bool()).unwrap_or(false);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowStart {
                    run_id,
                    flow_name,
                    parent_run_id,
                    parent_node_id,
                    spawned,
                    ts,
                });
            }
            "flow_node_start" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let node_id = v["node_id"].as_str().unwrap_or("").to_string();
                let label = v["label"].as_str().unwrap_or(&node_id).to_string();
                let parent_node_id = v["parent_node_id"].as_str().map(String::from);
                let kind = v
                    .get("kind")
                    .and_then(|k| serde_json::from_value(k.clone()).ok())
                    .unwrap_or(nodegraph::NodeKind::UserConfirm);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowNodeStart {
                    run_id,
                    node_id,
                    kind,
                    label,
                    parent_node_id,
                    ts,
                });
            }
            "flow_node_end" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let node_id = v["node_id"].as_str().unwrap_or("").to_string();
                let status: event::FlowNodeStatus = v
                    .get("status")
                    .and_then(|s| serde_json::from_value(s.clone()).ok())
                    .unwrap_or(event::FlowNodeStatus::Ok);
                let output_preview = v["output_preview"].as_str().map(String::from);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowNodeEnd {
                    run_id,
                    node_id,
                    status,
                    output_preview,
                    ts,
                });
            }
            "tool_node" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let parent_node_id = v["parent_node_id"].as_str().unwrap_or("").to_string();
                let tool_use_id = v["tool_use_id"].as_str().unwrap_or("").to_string();
                let tool_name = v["tool_name"].as_str().unwrap_or("").to_string();
                let args_preview = v["args_preview"].as_str().unwrap_or("").to_string();
                let call_intent = v
                    .get("call_intent")
                    .and_then(|value| serde_json::from_value(value.clone()).ok());
                let ts = parse_ts(v);
                out.push(TranscriptEntry::ToolNode {
                    run_id,
                    parent_node_id,
                    tool_use_id,
                    tool_name,
                    args_preview,
                    call_intent,
                    ts,
                });
            }
            "flow_end" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let ok = v["status"]["kind"].as_str() == Some("ok");
                let cancelled = v["status"]["kind"].as_str() == Some("cancelled");
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowDone {
                    run_id,
                    ok,
                    cancelled,
                    ts,
                });
            }
            "llm_call" => {
                let model = v["model"].as_str().unwrap_or("").to_string();
                let provider = v["provider"].as_str().unwrap_or("").to_string();
                let context_call_purpose = v
                    .get("context_call_purpose")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_default();
                let context_call_scope = v
                    .get("context_call_identity")
                    .and_then(|identity| identity.get("scope"))
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_else(|| {
                        if v["run_id"].is_null() {
                            crate::context_plan::ContextCallScope::Detached
                        } else {
                            crate::context_plan::ContextCallScope::Root
                        }
                    });
                let usage: provider::TokenUsage = v
                    .get("usage")
                    .and_then(|u| serde_json::from_value(u.clone()).ok())
                    .unwrap_or_default();
                let wallclock_ms = v["wallclock_ms"].as_u64().unwrap_or(0);
                let ttft_ms = v["ttft_ms"].as_u64();
                let tokens_per_second = v["tokens_per_second"].as_f64();
                let run_id = v["run_id"]
                    .as_str()
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
                    .map(event::FlowRunId);
                let node_id = v["node_id"].as_str().map(String::from);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::LlmCall {
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
                });
            }
            "tool_pending_approval" | "tool_approved" | "tool_denied" => {
                let Some(run_id) = v["run_id"]
                    .as_str()
                    .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
                    .map(event::FlowRunId)
                else {
                    continue;
                };
                let tool_use_id = v["tool_use_id"].as_str().unwrap_or_default().to_string();
                if tool_use_id.is_empty() {
                    continue;
                }
                let seq = v["seq"].as_u64().unwrap_or(0);
                let correlation = (run_id.0.to_string(), tool_use_id.clone());
                if canonical_permissions.contains(&correlation) {
                    continue;
                }
                let identity = if ty == "tool_pending_approval" {
                    let identity = crate::workflow::WorkflowPermissionIdentity::Legacy {
                        seq,
                        run_id: run_id.0.to_string(),
                        tool_use_id: tool_use_id.clone(),
                    };
                    legacy_pending
                        .entry(correlation.clone())
                        .or_default()
                        .push(identity.clone());
                    identity
                } else {
                    legacy_pending
                        .get_mut(&correlation)
                        .and_then(Vec::pop)
                        .unwrap_or_else(|| crate::workflow::WorkflowPermissionIdentity::Legacy {
                            seq,
                            run_id: run_id.0.to_string(),
                            tool_use_id: tool_use_id.clone(),
                        })
                };
                let state = match ty {
                    "tool_pending_approval" => crate::workflow::WorkflowPermissionState::Pending,
                    "tool_approved" => crate::workflow::WorkflowPermissionState::Approved,
                    "tool_denied" => crate::workflow::WorkflowPermissionState::Denied,
                    _ => unreachable!(),
                };
                let actor_label = match ty {
                    "tool_pending_approval" => "legacy approval actor unavailable",
                    "tool_approved" => "legacy approver unavailable",
                    "tool_denied" => "legacy denier unavailable",
                    _ => unreachable!(),
                };
                let at = parse_ts(v).unwrap_or_else(chrono::Utc::now);
                let payload = if state.is_pending() {
                    legacy_permission_payload(
                        run_id,
                        tool_use_id,
                        v["tool_name"].as_str().unwrap_or_default(),
                        v["reason"].as_str(),
                        actor_label,
                        at,
                    )
                } else if let Some(pending) = pending_permissions.get(&identity) {
                    let mut payload = pending.clone();
                    payload.actor = Some(
                        crate::permission_audit::PermissionProjectionActor::UnknownLegacy {
                            label: actor_label.into(),
                        },
                    );
                    payload.reason = v["reason"].as_str().map(String::from);
                    payload.at = at;
                    payload
                } else {
                    legacy_permission_payload(
                        run_id,
                        tool_use_id,
                        v["tool_name"].as_str().unwrap_or_default(),
                        v["reason"].as_str(),
                        actor_label,
                        at,
                    )
                };
                if state.is_pending() {
                    pending_permissions.insert(identity.clone(), payload.clone());
                } else {
                    pending_permissions.remove(&identity);
                }
                out.push(TranscriptEntry::PermissionRequest {
                    identity,
                    payload: Box::new(payload),
                    state,
                });
            }
            "permission_request_created"
            | "permission_request_targeted"
            | "permission_request_deferred"
            | "permission_request_approved"
            | "permission_request_denied"
            | "permission_request_cancelled"
            | "unrestricted_execution" => {
                let Some(payload) = v.get("payload").and_then(|payload| {
                    serde_json::from_value::<crate::permission_audit::PermissionRequestAudit>(
                        payload.clone(),
                    )
                    .ok()
                }) else {
                    continue;
                };
                let state = match ty {
                    "permission_request_created"
                    | "permission_request_targeted"
                    | "permission_request_deferred" => {
                        crate::workflow::WorkflowPermissionState::Pending
                    }
                    "permission_request_approved" => {
                        crate::workflow::WorkflowPermissionState::Approved
                    }
                    "permission_request_denied" => crate::workflow::WorkflowPermissionState::Denied,
                    "permission_request_cancelled" => {
                        crate::workflow::WorkflowPermissionState::Cancelled
                    }
                    "unrestricted_execution" => {
                        crate::workflow::WorkflowPermissionState::Unrestricted
                    }
                    _ => unreachable!(),
                };
                let Some(request_id) = payload.request_id.clone() else {
                    continue;
                };
                canonical_permissions.insert((
                    payload.requesting_run_id.0.to_string(),
                    payload.tool_use_id.clone(),
                ));
                let identity =
                    crate::workflow::WorkflowPermissionIdentity::Canonical { request_id };
                if state.is_pending() {
                    pending_permissions.insert(identity.clone(), payload.clone());
                } else {
                    pending_permissions.remove(&identity);
                }
                out.push(TranscriptEntry::PermissionRequest {
                    identity,
                    payload: Box::new(payload),
                    state,
                });
            }
            "permission_group_created"
            | "permission_group_updated"
            | "permission_group_resolved" => {
                if let Some(payload) = v.get("payload").and_then(|payload| {
                    serde_json::from_value::<crate::permission_audit::PermissionGroupAudit>(
                        payload.clone(),
                    )
                    .ok()
                }) {
                    out.push(TranscriptEntry::PermissionGroup {
                        payload,
                        resolved: ty == "permission_group_resolved",
                    });
                }
            }
            "terminal_final_state" => {
                let handle = v["handle"].as_str().unwrap_or("").to_string();
                if let Some(screen) = v.get("screen")
                    && let Ok(screen) =
                        serde_json::from_value::<crate::tools::term::TerminalScreen>(screen.clone())
                {
                    out.push(TranscriptEntry::TerminalFinalState { handle, screen });
                }
            }
            "mermaid_diagram" => {
                if let Some(source) = v.get("source").and_then(|s| s.as_str()) {
                    out.push(TranscriptEntry::MermaidDiagram {
                        source: source.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    out.extend(
        pending_permissions
            .into_iter()
            .map(|(identity, mut payload)| {
                payload.reason = Some("interrupted at end of persisted history".into());
                TranscriptEntry::PermissionRequest {
                    identity,
                    payload: Box::new(payload),
                    state: crate::workflow::WorkflowPermissionState::Interrupted,
                }
            }),
    );
    Ok(out)
}

pub(crate) fn project_transcript_records(
    records: &[crate::event_log::reader::ReplayRecord],
    ownership: &crate::event_log::replay::FlowOwnership,
) -> Vec<TranscriptEntry> {
    let mut patches: HashMap<u64, Vec<AttachmentPatch>> = HashMap::new();
    for record in records {
        if let crate::event::Event::AttachmentDegraded {
            message_seq,
            part_index,
            file_basename,
            reason,
            ..
        } = &record.envelope.event
        {
            patches
                .entry(*message_seq)
                .or_default()
                .push(AttachmentPatch {
                    part_index: *part_index,
                    file_basename: file_basename.clone(),
                    reason: reason.clone(),
                });
        }
    }
    let mut out = Vec::new();
    let mut messages = Vec::new();
    let mut message_positions = HashMap::new();
    let mut pending_permissions = std::collections::BTreeMap::<
        crate::workflow::WorkflowPermissionIdentity,
        crate::permission_audit::PermissionRequestAudit,
    >::new();
    let mut legacy_pending = std::collections::HashMap::<
        (String, String),
        Vec<crate::workflow::WorkflowPermissionIdentity>,
    >::new();
    let mut canonical_permissions = std::collections::HashSet::new();
    for record in records {
        let seq = record.envelope.seq;
        let ts = record.persisted_ts;
        match &record.envelope.event {
            crate::event::Event::UserMsg {
                message,
                flow_run_id,
                ..
            }
            | crate::event::Event::AssistantMsg {
                message,
                flow_run_id,
                ..
            }
            | crate::event::Event::ToolResultMsg {
                message,
                flow_run_id,
                ..
            }
            | crate::event::Event::SystemMsg {
                message,
                flow_run_id,
                ..
            } => {
                let mut message = message.clone();
                let belongs_to_root =
                    message_belongs_to_root(flow_run_id.as_ref(), &ownership.spawned);
                if let Some(patches) = patches.get(&seq) {
                    apply_attachment_patches(&mut message, patches);
                }
                let flow_run_id = flow_run_id.as_ref().and_then(|run_id| {
                    if ownership.known.contains(run_id) && !ownership.spawned.contains(run_id) {
                        None
                    } else {
                        Some(run_id.0.to_string())
                    }
                });
                let entry = TranscriptEntry::Message {
                    message,
                    flow_run_id,
                };
                if belongs_to_root {
                    push_transcript_message(
                        &mut out,
                        &mut messages,
                        &mut message_positions,
                        seq,
                        entry,
                    );
                } else {
                    out.push(entry);
                }
            }
            crate::event::Event::ContextCompact {
                flow_run_id,
                compacted_range_start,
                compacted_range_end,
                replacement_msg_seq,
                ..
            } => {
                if !message_belongs_to_root(flow_run_id.as_ref(), &ownership.spawned) {
                    continue;
                }
                let range_start = *compacted_range_start as usize;
                let range_end = *compacted_range_end as usize;
                let Some(replacement_seq) = replacement_msg_seq else {
                    continue;
                };
                compact_transcript_messages(
                    &mut out,
                    &mut messages,
                    &mut message_positions,
                    range_start,
                    range_end,
                    *replacement_seq,
                );
            }
            crate::event::Event::CompactionSummary {
                flow_run_id,
                range_start,
                range_end,
                compacted_count,
                before_tokens,
                after_tokens,
                summary,
                ..
            } => {
                if !message_belongs_to_root(flow_run_id.as_ref(), &ownership.spawned) {
                    continue;
                }
                out.push(TranscriptEntry::CompactionSummary {
                    range_start: *range_start as usize,
                    range_end: *range_end as usize,
                    compacted_count: *compacted_count,
                    before_tokens: *before_tokens,
                    after_tokens: *after_tokens,
                    summary: summary.clone(),
                    ts,
                });
            }
            crate::event::Event::DiffPreview {
                tool_use_id,
                title,
                old_content,
                new_content,
                unified_diff,
                ..
            } => out.push(TranscriptEntry::DiffPreview {
                tool_use_id: tool_use_id.clone(),
                title: title.clone(),
                old_content: old_content.clone(),
                new_content: new_content.clone(),
                unified_diff: unified_diff.clone(),
            }),
            crate::event::Event::FileEditApplied {
                turn_id,
                flow_run_id,
                tool_use_id,
                tool_name,
                path,
                metrics,
            } => out.push(TranscriptEntry::FileEditApplied {
                turn_id: turn_id.as_ref().map(ToString::to_string),
                flow_run_id: flow_run_id.as_ref().map(ToString::to_string),
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                path: path.clone(),
                metrics: *metrics,
            }),
            crate::event::Event::FlowGraph { run_id, graph } => {
                out.push(TranscriptEntry::FlowGraph {
                    run_id: run_id.0.to_string(),
                    flow_name: graph.flow_name.clone(),
                    graph: graph.clone(),
                    ts,
                });
            }
            crate::event::Event::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                spawned,
            } => out.push(TranscriptEntry::FlowStart {
                run_id: run_id.0.to_string(),
                flow_name: flow_name.clone(),
                parent_run_id: parent_run_id.as_ref().map(|run_id| run_id.0.to_string()),
                parent_node_id: parent_node_id.clone(),
                spawned: *spawned,
                ts,
            }),
            crate::event::Event::FlowNodeStart {
                run_id,
                node_id,
                kind,
                label,
                parent_node_id,
            } => out.push(TranscriptEntry::FlowNodeStart {
                run_id: run_id.0.to_string(),
                node_id: node_id.clone(),
                kind: kind.clone(),
                label: if label.is_empty() {
                    node_id.clone()
                } else {
                    label.clone()
                },
                parent_node_id: parent_node_id.clone(),
                ts,
            }),
            crate::event::Event::FlowNodeEnd {
                run_id,
                node_id,
                status,
                output_preview,
            } => out.push(TranscriptEntry::FlowNodeEnd {
                run_id: run_id.0.to_string(),
                node_id: node_id.clone(),
                status: status.clone(),
                output_preview: output_preview.clone(),
                ts,
            }),
            crate::event::Event::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool_name,
                args_preview,
                call_intent,
            } => out.push(TranscriptEntry::ToolNode {
                run_id: run_id.0.to_string(),
                parent_node_id: parent_node_id.clone(),
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                args_preview: args_preview.clone(),
                call_intent: call_intent.clone(),
                ts,
            }),
            crate::event::Event::FlowEnd { run_id, status, .. } => {
                out.push(TranscriptEntry::FlowDone {
                    run_id: run_id.0.to_string(),
                    ok: matches!(status, crate::event::FlowStatus::Ok),
                    cancelled: matches!(status, crate::event::FlowStatus::Cancelled),
                    ts,
                });
            }
            crate::event::Event::LlmCall {
                model,
                provider,
                context_call_purpose,
                context_call_identity,
                usage,
                wallclock_ms,
                ttft_ms,
                tokens_per_second,
                run_id,
                node_id,
                ..
            } => {
                let context_call_scope = context_call_identity.as_ref().map_or_else(
                    || {
                        if run_id.is_none() {
                            crate::context_plan::ContextCallScope::Detached
                        } else {
                            crate::context_plan::ContextCallScope::Root
                        }
                    },
                    |identity| identity.scope,
                );
                out.push(TranscriptEntry::LlmCall {
                    model: model.clone(),
                    provider: provider.clone(),
                    context_call_purpose: context_call_purpose.unwrap_or_default(),
                    context_call_scope,
                    usage: usage.clone(),
                    wallclock_ms: *wallclock_ms,
                    ttft_ms: *ttft_ms,
                    tokens_per_second: *tokens_per_second,
                    run_id: run_id.clone(),
                    node_id: node_id.clone(),
                    ts,
                });
            }
            crate::event::Event::ToolPendingApproval {
                run_id,
                tool_use_id,
                ..
            }
            | crate::event::Event::ToolApproved {
                run_id,
                tool_use_id,
                ..
            }
            | crate::event::Event::ToolDenied {
                run_id,
                tool_use_id,
                ..
            } => {
                if tool_use_id.is_empty() {
                    continue;
                }
                let correlation = (run_id.0.to_string(), tool_use_id.clone());
                if canonical_permissions.contains(&correlation) {
                    continue;
                }
                let (state, actor_label, tool_name, reason) = match &record.envelope.event {
                    crate::event::Event::ToolPendingApproval { tool_name, .. } => (
                        crate::workflow::WorkflowPermissionState::Pending,
                        "legacy approval actor unavailable",
                        tool_name.as_str(),
                        None,
                    ),
                    crate::event::Event::ToolApproved { .. } => (
                        crate::workflow::WorkflowPermissionState::Approved,
                        "legacy approver unavailable",
                        "",
                        None,
                    ),
                    crate::event::Event::ToolDenied { reason, .. } => (
                        crate::workflow::WorkflowPermissionState::Denied,
                        "legacy denier unavailable",
                        "",
                        Some(reason.as_str()),
                    ),
                    _ => unreachable!(),
                };
                let pending = state.is_pending();
                let identity = if pending {
                    let identity = crate::workflow::WorkflowPermissionIdentity::Legacy {
                        seq,
                        run_id: run_id.0.to_string(),
                        tool_use_id: tool_use_id.clone(),
                    };
                    legacy_pending
                        .entry(correlation.clone())
                        .or_default()
                        .push(identity.clone());
                    identity
                } else {
                    legacy_pending
                        .get_mut(&correlation)
                        .and_then(Vec::pop)
                        .unwrap_or_else(|| crate::workflow::WorkflowPermissionIdentity::Legacy {
                            seq,
                            run_id: run_id.0.to_string(),
                            tool_use_id: tool_use_id.clone(),
                        })
                };
                let at = ts.unwrap_or_else(chrono::Utc::now);
                let payload = if state.is_pending() {
                    legacy_permission_payload(
                        run_id.clone(),
                        tool_use_id.clone(),
                        tool_name,
                        reason,
                        actor_label,
                        at,
                    )
                } else if let Some(pending) = pending_permissions.get(&identity) {
                    let mut payload = pending.clone();
                    payload.actor = Some(
                        crate::permission_audit::PermissionProjectionActor::UnknownLegacy {
                            label: actor_label.into(),
                        },
                    );
                    payload.reason = reason.map(String::from);
                    payload.at = at;
                    payload
                } else {
                    legacy_permission_payload(
                        run_id.clone(),
                        tool_use_id.clone(),
                        tool_name,
                        reason,
                        actor_label,
                        at,
                    )
                };
                if state.is_pending() {
                    pending_permissions.insert(identity.clone(), payload.clone());
                } else {
                    pending_permissions.remove(&identity);
                }
                out.push(TranscriptEntry::PermissionRequest {
                    identity,
                    payload: Box::new(payload),
                    state,
                });
            }
            crate::event::Event::PermissionRequestCreated { payload }
            | crate::event::Event::PermissionRequestTargeted { payload }
            | crate::event::Event::PermissionRequestDeferred { payload }
            | crate::event::Event::PermissionRequestApproved { payload }
            | crate::event::Event::PermissionRequestDenied { payload }
            | crate::event::Event::PermissionRequestCancelled { payload }
            | crate::event::Event::UnrestrictedExecution { payload } => {
                let state = match &record.envelope.event {
                    crate::event::Event::PermissionRequestCreated { .. }
                    | crate::event::Event::PermissionRequestTargeted { .. }
                    | crate::event::Event::PermissionRequestDeferred { .. } => {
                        crate::workflow::WorkflowPermissionState::Pending
                    }
                    crate::event::Event::PermissionRequestApproved { .. } => {
                        crate::workflow::WorkflowPermissionState::Approved
                    }
                    crate::event::Event::PermissionRequestDenied { .. } => {
                        crate::workflow::WorkflowPermissionState::Denied
                    }
                    crate::event::Event::PermissionRequestCancelled { .. } => {
                        crate::workflow::WorkflowPermissionState::Cancelled
                    }
                    crate::event::Event::UnrestrictedExecution { .. } => {
                        crate::workflow::WorkflowPermissionState::Unrestricted
                    }
                    _ => unreachable!(),
                };
                let Some(request_id) = payload.request_id.clone() else {
                    continue;
                };
                canonical_permissions.insert((
                    payload.requesting_run_id.0.to_string(),
                    payload.tool_use_id.clone(),
                ));
                let identity =
                    crate::workflow::WorkflowPermissionIdentity::Canonical { request_id };
                if state.is_pending() {
                    pending_permissions.insert(identity.clone(), payload.clone());
                } else {
                    pending_permissions.remove(&identity);
                }
                out.push(TranscriptEntry::PermissionRequest {
                    identity,
                    payload: Box::new(payload.clone()),
                    state,
                });
            }
            crate::event::Event::PermissionGroupCreated { payload }
            | crate::event::Event::PermissionGroupUpdated { payload }
            | crate::event::Event::PermissionGroupResolved { payload } => {
                out.push(TranscriptEntry::PermissionGroup {
                    payload: payload.clone(),
                    resolved: matches!(
                        &record.envelope.event,
                        crate::event::Event::PermissionGroupResolved { .. }
                    ),
                });
            }
            crate::event::Event::TerminalFinalState { handle, screen, .. } => {
                out.push(TranscriptEntry::TerminalFinalState {
                    handle: handle.clone(),
                    screen: screen.clone(),
                });
            }
            crate::event::Event::MermaidDiagram { source } => {
                out.push(TranscriptEntry::MermaidDiagram {
                    source: source.clone(),
                });
            }
            _ => {}
        }
    }
    out.extend(
        pending_permissions
            .into_iter()
            .map(|(identity, mut payload)| {
                payload.reason = Some("interrupted at end of persisted history".into());
                TranscriptEntry::PermissionRequest {
                    identity,
                    payload: Box::new(payload),
                    state: crate::workflow::WorkflowPermissionState::Interrupted,
                }
            }),
    );
    out
}

pub trait MessageProjection {
    fn to_messages(&self) -> Vec<Message>;
    fn to_messages_with_seq(&self) -> Vec<(u64, Message)>;
}

impl MessageProjection for [crate::event::EventEnvelope] {
    fn to_messages(&self) -> Vec<Message> {
        self.to_messages_with_seq()
            .into_iter()
            .map(|(_, msg)| msg)
            .collect()
    }

    fn to_messages_with_seq(&self) -> Vec<(u64, Message)> {
        let spawned_flow_ids = spawned_flow_ids(self);
        let mut acc: Vec<(u64, Message)> = Vec::new();
        let mut positions = HashMap::new();
        for env in self {
            apply_envelope_to_messages(env, &spawned_flow_ids, &mut acc, &mut positions);
        }
        acc
    }
}

pub(crate) fn spawned_flow_ids(
    envelopes: &[crate::event::EventEnvelope],
) -> std::collections::HashSet<crate::event::FlowRunId> {
    let mut children =
        std::collections::HashMap::<crate::event::FlowRunId, Vec<crate::event::FlowRunId>>::new();
    let mut spawned = std::collections::HashSet::new();
    for env in envelopes {
        if let crate::event::Event::FlowStart {
            run_id,
            parent_run_id,
            spawned: is_spawned,
            ..
        } = &env.event
        {
            if let Some(parent_run_id) = parent_run_id {
                children
                    .entry(parent_run_id.clone())
                    .or_default()
                    .push(run_id.clone());
            }
            if *is_spawned {
                spawned.insert(run_id.clone());
            }
        }
    }
    let mut queue = std::collections::VecDeque::from_iter(spawned.iter().cloned());
    while let Some(parent) = queue.pop_front() {
        if let Some(descendants) = children.get(&parent) {
            for descendant in descendants {
                if spawned.insert(descendant.clone()) {
                    queue.push_back(descendant.clone());
                }
            }
        }
    }
    spawned
}

pub(crate) fn message_positions(acc: &[(u64, Message)]) -> HashMap<u64, usize> {
    acc.iter()
        .enumerate()
        .map(|(index, (seq, _))| (*seq, index))
        .collect()
}

fn rebuild_message_positions(acc: &[(u64, Message)], positions: &mut HashMap<u64, usize>) {
    positions.clear();
    positions.extend(
        acc.iter()
            .enumerate()
            .map(|(index, (seq, _))| (*seq, index)),
    );
}

pub(crate) fn apply_attachment_degradation(
    acc: &mut [(u64, Message)],
    positions: &HashMap<u64, usize>,
    message_seq: u64,
    part_index: usize,
    file_basename: &str,
    reason: &str,
) -> bool {
    let Some(message_index) = positions.get(&message_seq).copied() else {
        return false;
    };
    let Some(part) = acc
        .get_mut(message_index)
        .and_then(|(_, message)| message.parts.get_mut(part_index))
    else {
        return false;
    };
    let replacement = MessagePart::Text {
        text: format!("[attachment unavailable: {} — {}]", file_basename, reason),
    };
    if *part == replacement {
        return false;
    }
    *part = replacement;
    true
}

pub(crate) fn message_belongs_to_root(
    flow_run_id: Option<&crate::event::FlowRunId>,
    spawned_flow_ids: &std::collections::HashSet<crate::event::FlowRunId>,
) -> bool {
    flow_run_id.is_none_or(|run_id| !spawned_flow_ids.contains(run_id))
}

pub(crate) fn apply_envelope_to_messages(
    env: &crate::event::EventEnvelope,
    spawned_flow_ids: &std::collections::HashSet<crate::event::FlowRunId>,
    acc: &mut Vec<(u64, Message)>,
    positions: &mut HashMap<u64, usize>,
) -> bool {
    match &env.event {
        crate::event::Event::UserMsg {
            message,
            flow_run_id,
            ..
        }
        | crate::event::Event::AssistantMsg {
            message,
            flow_run_id,
            ..
        }
        | crate::event::Event::ToolResultMsg {
            message,
            flow_run_id,
            ..
        } if message_belongs_to_root(flow_run_id.as_ref(), spawned_flow_ids) => {
            positions.insert(env.seq, acc.len());
            acc.push((env.seq, message.clone()));
            true
        }
        crate::event::Event::SystemMsg {
            message,
            flow_run_id,
            ..
        } if message_belongs_to_root(flow_run_id.as_ref(), spawned_flow_ids) => {
            positions.insert(env.seq, acc.len());
            acc.push((env.seq, message.clone()));
            true
        }
        crate::event::Event::ContextCompact {
            flow_run_id,
            compacted_range_start,
            compacted_range_end,
            replacement_msg_seq,
            summary_text,
            after_tokens,
            before_tokens,
            ..
        } if message_belongs_to_root(flow_run_id.as_ref(), spawned_flow_ids) => {
            let range_start = *compacted_range_start as usize;
            let range_end = *compacted_range_end as usize;
            if range_start > range_end || range_end >= acc.len() {
                return false;
            }
            let Some(rep_seq) = replacement_msg_seq else {
                return false;
            };
            let Some(rep_idx) = positions.get(rep_seq).copied() else {
                return false;
            };
            if *after_tokens >= *before_tokens {
                return false;
            }
            let removed_count = range_end - range_start + 1;
            let replacement = if let Some(summary) = summary_text {
                (
                    *rep_seq,
                    Message::system_compact_summary(
                        crate::event::TurnId::now(),
                        summary.clone(),
                        range_start as u64,
                        range_end as u64,
                        removed_count,
                    ),
                )
            } else {
                acc[rep_idx].clone()
            };
            let (adjusted_start, adjusted_end) = if rep_idx < range_start {
                acc.remove(rep_idx);
                (range_start - 1, range_end - 1)
            } else if rep_idx > range_end {
                acc.remove(rep_idx);
                (range_start, range_end)
            } else {
                (range_start, range_end)
            };
            acc.splice(adjusted_start..=adjusted_end, [replacement]);
            rebuild_message_positions(acc, positions);
            true
        }
        crate::event::Event::Checkpoint {
            flow_run_id,
            messages,
            ..
        } if message_belongs_to_root(flow_run_id.as_ref(), spawned_flow_ids) => {
            let checkpoint = messages
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, message)| (u64::MAX.saturating_sub(index as u64), message))
                .collect::<Vec<_>>();
            if *acc == checkpoint {
                false
            } else {
                *acc = checkpoint;
                rebuild_message_positions(acc, positions);
                true
            }
        }
        crate::event::Event::AttachmentDegraded {
            message_seq,
            part_index,
            file_basename,
            reason,
            ..
        } => apply_attachment_degradation(
            acc,
            positions,
            *message_seq,
            *part_index,
            file_basename,
            reason,
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::event::{Event, EventEnvelope, FlowRunId, TurnId};
    use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
    use uuid::Uuid;

    fn message(role: MessageRole, text: &str) -> Message {
        Message {
            role,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        }
    }

    fn flow_start(run_id: FlowRunId, parent_run_id: Option<FlowRunId>, spawned: bool) -> Event {
        Event::FlowStart {
            run_id,
            flow_name: "test".into(),
            spawned,
            parent_run_id,
            parent_node_id: None,
        }
    }

    fn canonical_permission_payload(
        run_id: FlowRunId,
        tool_use_id: &str,
        actor: crate::permission_audit::PermissionProjectionActor,
    ) -> crate::permission_audit::PermissionRequestAudit {
        crate::permission_audit::PermissionRequestAudit {
            request_id: Some(crate::permission::PermissionRequestId(Uuid::now_v7())),
            revision: 1,
            session_id: "session".into(),
            requesting_run_id: run_id.clone(),
            parent_run_id: None,
            root_run_id: run_id,
            tool_use_id: tool_use_id.into(),
            tool: "fs.read".into(),
            call_intent: None,
            tier: crate::tool::Tier::Two,
            execution_boundary: Default::default(),
            provenance: Default::default(),
            target: crate::permission_audit::PermissionAuditTarget::User,
            group_ids: Vec::new(),
            policy: crate::permission_audit::PermissionPolicyReference {
                snapshot_id: "snapshot".into(),
                rule_id: "rule".into(),
            },
            escalation_path: Vec::new(),
            decision_id: Some(format!("decision-{tool_use_id}")),
            actor: Some(actor),
            scope: None,
            reason: None,
            at: chrono::Utc::now(),
        }
    }

    #[test]
    fn replay_excludes_subagent_messages() {
        let root = FlowRunId(Uuid::now_v7());
        let child = FlowRunId(Uuid::now_v7());
        let envelopes = vec![
            EventEnvelope::new(1, flow_start(root.clone(), None, false)),
            EventEnvelope::new(2, flow_start(child.clone(), Some(root), true)),
            EventEnvelope::new(
                3,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(child),
                    message: message(MessageRole::Assistant, "child"),
                },
            ),
        ];
        assert!(super::MessageProjection::to_messages(envelopes.as_slice()).is_empty());
    }

    #[test]
    fn transcript_replay_ignores_spawned_compaction_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let child = FlowRunId::now();
        let events = [
            EventEnvelope::new(
                1,
                Event::UserMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: None,
                    message: message(MessageRole::User, "root user"),
                },
            ),
            EventEnvelope::new(2, flow_start(child.clone(), None, true)),
            EventEnvelope::new(
                3,
                Event::SystemMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(child.clone()),
                    message: Message::system_compact_summary(
                        TurnId::now(),
                        "child summary",
                        0,
                        0,
                        1,
                    ),
                },
            ),
            EventEnvelope::new(
                4,
                Event::ContextCompact {
                    session_id: "session".into(),
                    flow_run_id: Some(child.clone()),
                    before_tokens: 100,
                    after_tokens: 10,
                    compacted_range_start: 0,
                    compacted_range_end: 0,
                    summary_text: Some("child summary".into()),
                    replacement_msg_seq: Some(3),
                },
            ),
            EventEnvelope::new(
                5,
                Event::CompactionSummary {
                    session_id: "session".into(),
                    flow_run_id: Some(child),
                    range_start: 0,
                    range_end: 0,
                    compacted_count: 1,
                    before_tokens: 100,
                    after_tokens: 10,
                    summary: "child summary".into(),
                },
            ),
        ];
        let jsonl = events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let raw_entries = super::replay_transcript_from_raw(&path).unwrap();
        let entries = super::replay_transcript_from(&path).unwrap();
        assert_eq!(format!("{raw_entries:#?}"), format!("{entries:#?}"));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            super::TranscriptEntry::Message { message, flow_run_id: None }
                if message.text_concat() == "root user"
        )));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            super::TranscriptEntry::FlowStart { spawned: true, .. }
        )));
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry, super::TranscriptEntry::CompactionSummary { .. }))
        );
    }

    #[test]
    fn transcript_compaction_preserves_interleaved_non_message_entries() {
        let spawned = FlowRunId::now();
        let events = [
            EventEnvelope::new(
                1,
                Event::UserMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: None,
                    message: message(MessageRole::User, "old user"),
                },
            ),
            EventEnvelope::new(2, flow_start(spawned.clone(), None, true)),
            EventEnvelope::new(
                3,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(spawned.clone()),
                    message: message(MessageRole::Assistant, "spawned assistant"),
                },
            ),
            EventEnvelope::new(
                4,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: None,
                    message: message(MessageRole::Assistant, "old assistant"),
                },
            ),
            EventEnvelope::new(
                5,
                Event::SystemMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: None,
                    message: Message::system_compact_summary(TurnId::now(), "summary", 0, 1, 2),
                },
            ),
            EventEnvelope::new(
                6,
                Event::ContextCompact {
                    session_id: "session".into(),
                    flow_run_id: None,
                    before_tokens: 100,
                    after_tokens: 10,
                    compacted_range_start: 0,
                    compacted_range_end: 1,
                    summary_text: Some("summary".into()),
                    replacement_msg_seq: Some(5),
                },
            ),
        ];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(
            &path,
            events
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .join("\n"),
        )
        .unwrap();
        let raw_entries = super::replay_transcript_from_raw(&path).unwrap();
        let entries = super::replay_transcript_from(&path).unwrap();
        assert_eq!(format!("{raw_entries:#?}"), format!("{entries:#?}"));
        assert_eq!(entries.len(), 3);
        assert!(matches!(
            &entries[0],
            super::TranscriptEntry::Message { message, .. }
                if message.text_concat() == "summary"
        ));
        assert!(matches!(
            &entries[1],
            super::TranscriptEntry::FlowStart { run_id, .. } if run_id == &spawned.0.to_string()
        ));
        assert!(matches!(
            &entries[2],
            super::TranscriptEntry::Message { message, flow_run_id: Some(run_id) }
                if message.text_concat() == "spawned assistant"
                    && run_id == &spawned.0.to_string()
        ));
    }

    #[test]
    fn late_attachment_degradation_updates_all_replay_views() {
        use crate::message::{ImageData, ImageSource};
        use crate::provider::ImageDetail;

        let image = Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Image {
                source: ImageSource {
                    media_type: "image/png".into(),
                    data: ImageData::Path {
                        path: "/tmp/missing.png".into(),
                    },
                    detail: ImageDetail::Auto,
                },
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        };
        let events = vec![
            EventEnvelope::new(
                1,
                Event::UserMsg {
                    turn_id: image.turn_id.clone(),
                    flow_run_id: None,
                    message: image,
                },
            ),
            EventEnvelope::new(
                2,
                Event::AttachmentDegraded {
                    turn_id: None,
                    flow_run_id: None,
                    message_seq: 1,
                    part_index: 0,
                    file_basename: "missing.png".into(),
                    reason: "unreadable".into(),
                },
            ),
        ];
        let jsonl = events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        let replay =
            crate::event_log::replay::SessionReplay::from_reader(std::io::Cursor::new(jsonl), None)
                .unwrap();

        assert!(
            replay.compacted_messages[0]
                .1
                .text_concat()
                .contains("missing.png")
        );
        assert!(
            replay.all_messages[0]
                .1
                .text_concat()
                .contains("missing.png")
        );
        let transcript = crate::event_log::replay::transcript_from_envelopes(&events);
        assert!(matches!(
            &transcript[0],
            super::TranscriptEntry::Message { message, .. }
                if message.text_concat().contains("missing.png")
        ));
    }

    #[test]
    fn streaming_transcript_preserves_missing_legacy_optional_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let run_id = Uuid::now_v7();
        let turn_id = TurnId::now();
        let lines = [
            serde_json::json!({
                "type": "flow_start",
                "seq": 1,
                "run_id": run_id,
            }),
            serde_json::json!({
                "type": "flow_node_start",
                "seq": 2,
                "run_id": run_id,
                "node_id": "legacy-node",
            }),
            serde_json::json!({
                "type": "assistant_msg",
                "seq": 3,
                "turn_id": turn_id,
                "message": message(MessageRole::Assistant, "legacy"),
            }),
        ];
        std::fs::write(
            &path,
            lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let raw_entries = super::replay_transcript_from_raw(&path).unwrap();
        let entries = super::replay_transcript_from(&path).unwrap();

        assert_eq!(format!("{raw_entries:#?}"), format!("{entries:#?}"));
    }

    #[test]
    fn replay_excludes_ordinary_subflow_execution_messages() {
        let root = FlowRunId(Uuid::now_v7());
        let child = FlowRunId(Uuid::now_v7());
        let envelopes = vec![
            EventEnvelope::new(1, flow_start(root.clone(), None, false)),
            EventEnvelope::new(2, flow_start(child.clone(), Some(root), false)),
            EventEnvelope::new(
                3,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(child),
                    message: message(MessageRole::Assistant, "ordinary child"),
                },
            ),
        ];
        let messages = super::MessageProjection::to_messages(envelopes.as_slice());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text_concat(), "ordinary child");
    }

    #[test]
    fn replay_excludes_descendants_of_spawned_flows() {
        let root = FlowRunId(Uuid::now_v7());
        let spawned = FlowRunId(Uuid::now_v7());
        let descendant = FlowRunId(Uuid::now_v7());
        let envelopes = vec![
            EventEnvelope::new(1, flow_start(root.clone(), None, false)),
            EventEnvelope::new(2, flow_start(spawned.clone(), Some(root), true)),
            EventEnvelope::new(3, flow_start(descendant.clone(), Some(spawned), false)),
            EventEnvelope::new(
                4,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(descendant),
                    message: message(MessageRole::Assistant, "spawned descendant"),
                },
            ),
        ];
        assert!(super::MessageProjection::to_messages(envelopes.as_slice()).is_empty());
    }

    #[test]
    fn replay_keeps_unknown_nonspawned_message() {
        let orphan = FlowRunId(Uuid::now_v7());
        let envelopes = vec![EventEnvelope::new(
            1,
            Event::AssistantMsg {
                turn_id: TurnId::now(),
                flow_run_id: Some(orphan),
                message: message(MessageRole::Assistant, "orphan"),
            },
        )];
        let messages = super::MessageProjection::to_messages(envelopes.as_slice());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text_concat(), "orphan");
    }

    #[test]
    fn s8_immediate_canonical_decisions_suppress_paired_legacy_identities() {
        use crate::permission_audit::PermissionProjectionActor;
        use crate::workflow::{WorkflowPermissionIdentity, WorkflowPermissionState};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let run = FlowRunId(Uuid::now_v7());
        let cases = [
            (
                "auto",
                "permission_request_approved",
                "tool_approved",
                PermissionProjectionActor::Policy {
                    policy_version: "snapshot".into(),
                    rule_id: "auto".into(),
                },
                WorkflowPermissionState::Approved,
            ),
            (
                "grant",
                "permission_request_approved",
                "tool_approved",
                PermissionProjectionActor::User {
                    session_id: "session".into(),
                    principal_id: Some("grant-owner".into()),
                },
                WorkflowPermissionState::Approved,
            ),
            (
                "denied",
                "permission_request_denied",
                "tool_denied",
                PermissionProjectionActor::Policy {
                    policy_version: "snapshot".into(),
                    rule_id: "deny".into(),
                },
                WorkflowPermissionState::Denied,
            ),
            (
                "unrestricted",
                "unrestricted_execution",
                "tool_approved",
                PermissionProjectionActor::Policy {
                    policy_version: "snapshot".into(),
                    rule_id: "unrestricted".into(),
                },
                WorkflowPermissionState::Unrestricted,
            ),
        ];
        let mut lines = Vec::new();
        for (index, (tool_use_id, canonical_type, legacy_type, actor, _)) in
            cases.iter().enumerate()
        {
            let payload = canonical_permission_payload(run.clone(), tool_use_id, actor.clone());
            let seq = (index * 2 + 1) as u64;
            lines.push(
                serde_json::json!({"type":canonical_type,"seq":seq,"payload":payload}).to_string(),
            );
            lines.push(
                serde_json::json!({"type":legacy_type,"seq":seq + 1,"run_id":run.0.to_string(),"tool_use_id":tool_use_id,"decided_by":"legacy-adapter","reason":"legacy denial"})
                    .to_string(),
            );
        }
        std::fs::write(&path, lines.join("\n")).unwrap();

        let raw_entries = super::replay_transcript_from_raw(&path).unwrap();
        let entries = super::replay_transcript_from(&path).unwrap();
        assert_eq!(format!("{raw_entries:#?}"), format!("{entries:#?}"));
        let permissions = entries
            .iter()
            .filter_map(|entry| match entry {
                super::TranscriptEntry::PermissionRequest {
                    identity,
                    payload,
                    state,
                } => Some((identity, payload, state)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(permissions.len(), cases.len());
        for ((identity, payload, state), (tool_use_id, _, _, actor, expected_state)) in
            permissions.into_iter().zip(cases)
        {
            assert!(matches!(
                identity,
                WorkflowPermissionIdentity::Canonical { .. }
            ));
            assert_eq!(payload.tool_use_id, tool_use_id);
            assert_eq!(payload.actor.as_ref(), Some(&actor));
            assert_eq!(*state, expected_state);
        }
    }

    #[test]
    fn legacy_approval_replay_correlates_repeated_exact_tool_lifecycles() {
        use crate::permission_audit::PermissionProjectionActor;
        use crate::workflow::{WorkflowPermissionIdentity, WorkflowPermissionState};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let run = Uuid::now_v7().to_string();
        let lines = [
            serde_json::json!({"type":"tool_pending_approval","seq":10,"run_id":run,"tool_use_id":"same","tool_name":"fs.write","args_preview":"{}","level":"approve"}),
            serde_json::json!({"type":"tool_approved","seq":11,"run_id":run,"tool_use_id":"same","decided_by":"user"}),
            serde_json::json!({"type":"tool_pending_approval","seq":12,"run_id":run,"tool_use_id":"same","tool_name":"fs.edit","args_preview":"{}","level":"approve"}),
            serde_json::json!({"type":"tool_denied","seq":13,"run_id":run,"tool_use_id":"same","reason":"no"}),
            serde_json::json!({"type":"tool_approved","seq":14,"run_id":run,"tool_use_id":"orphan","decided_by":"user"}),
        ];
        std::fs::write(
            &path,
            lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let entries = super::replay_transcript_from(&path).unwrap();
        let finals = entries
            .iter()
            .filter_map(|entry| match entry {
                super::TranscriptEntry::PermissionRequest {
                    identity,
                    payload,
                    state,
                } if !state.is_pending() => Some((identity, payload, state)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finals.len(), 3);
        assert_eq!(
            finals[0].0,
            &WorkflowPermissionIdentity::Legacy {
                seq: 10,
                run_id: run.clone(),
                tool_use_id: "same".into(),
            }
        );
        assert_eq!(finals[0].1.tool, "fs.write");
        assert_eq!(*finals[0].2, WorkflowPermissionState::Approved);
        assert_eq!(
            finals[1].0,
            &WorkflowPermissionIdentity::Legacy {
                seq: 12,
                run_id: run.clone(),
                tool_use_id: "same".into(),
            }
        );
        assert_eq!(finals[1].1.tool, "fs.edit");
        assert_eq!(*finals[1].2, WorkflowPermissionState::Denied);
        assert_eq!(
            finals[2].0,
            &WorkflowPermissionIdentity::Legacy {
                seq: 14,
                run_id: run,
                tool_use_id: "orphan".into(),
            }
        );
        assert!(finals.iter().all(|(_, payload, _)| {
            payload.request_id.is_none()
                && matches!(
                    payload.actor,
                    Some(PermissionProjectionActor::UnknownLegacy { .. })
                )
        }));
    }

    #[test]
    fn unresolved_legacy_pending_replays_as_interrupted_with_origin_identity() {
        use crate::workflow::{WorkflowPermissionIdentity, WorkflowPermissionState};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let run = Uuid::now_v7().to_string();
        std::fs::write(
            &path,
            serde_json::json!({"type":"tool_pending_approval","seq":21,"run_id":run,"tool_use_id":"pending","tool_name":"bash.spawn","args_preview":"{}","level":"approve"}).to_string(),
        ).unwrap();

        let entries = super::replay_transcript_from(&path).unwrap();
        let (identity, payload) = entries
            .iter()
            .find_map(|entry| match entry {
                super::TranscriptEntry::PermissionRequest {
                    identity,
                    payload,
                    state: WorkflowPermissionState::Interrupted,
                } => Some((identity, payload)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            identity,
            &WorkflowPermissionIdentity::Legacy {
                seq: 21,
                run_id: run,
                tool_use_id: "pending".into(),
            }
        );
        assert_eq!(payload.tool, "bash.spawn");
    }
}
